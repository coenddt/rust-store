//! 权限（读 / 写 / 字段裁剪 / owner 条件）方法。

use napi::Result;
use napi_derive::napi;
use serde_json::Value;

use rust_store_core::permission::{
    can_read_schema, can_write_schema, context_from_value, filter_writable_data,
    get_readable_computes, get_readable_fields, get_readable_relations, get_writable_fields,
    merge_owner_condition, should_inject_owner_condition,
};

use crate::convert::{err, sorted_set};
use crate::Registry;

#[napi]
impl Registry {
    // ─── 权限 ─────────────────────────────────────────────

    #[napi]
    pub fn can_read(&self, model: String, ctx: Option<Value>) -> Result<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(can_read_schema(schema, context.as_ref()))
    }

    #[napi]
    pub fn can_write(&self, model: String, ctx: Option<Value>) -> Result<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(can_write_schema(schema, context.as_ref()))
    }

    #[napi]
    pub fn should_inject_owner(&self, model: String, ctx: Option<Value>) -> Result<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(should_inject_owner_condition(schema, context.as_ref()))
    }

    /// 非 admin 用户只看自己数据时叠加 owner 条件；无上下文时原样返回
    #[napi]
    pub fn merge_owner_condition(
        &self,
        model: String,
        ctx: Option<Value>,
        condition: Option<Value>,
    ) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(merge_owner_condition(schema, context.as_ref(), condition).unwrap_or(Value::Null))
    }

    #[napi]
    pub fn readable_fields(&self, model: String, ctx: Option<Value>) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(sorted_set(get_readable_fields(schema, context.as_ref())))
    }

    #[napi]
    pub fn readable_relations(&self, model: String, ctx: Option<Value>) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(sorted_set(get_readable_relations(schema, context.as_ref())))
    }

    /// 可读计算列（read 白名单判决与 fields/relations 同构；`ctx=None` → None 不裁剪）
    #[napi]
    pub fn readable_computes(&self, model: String, ctx: Option<Value>) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(sorted_set(get_readable_computes(schema, context.as_ref())))
    }

    #[napi]
    pub fn writable_fields(&self, model: String, ctx: Option<Value>) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(sorted_set(get_writable_fields(schema, context.as_ref())))
    }

    #[napi]
    pub fn filter_writable_data(
        &self,
        model: String,
        ctx: Option<Value>,
        data: Value,
    ) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(filter_writable_data(schema, context.as_ref(), &data))
    }

    // ─── RBAC 查询面（判决唯一在 core；本层零判决逻辑，对齐 core-py 同名方法） ──

    /// RBAC 动作判决：`action ∈ {read, insert, update, remove}`。
    /// 策略未注入 / RBAC 不介入 → true（与 plan 链路的实际拦截结果一致）。
    #[napi]
    pub fn rbac_can(&self, model: String, action: String, ctx: Option<Value>) -> Result<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(match action.as_str() {
            "read" => rust_store_core::rbac::ensure_read(&self.core, schema, context.as_ref())
                .is_ok(),
            a => {
                let wa = rust_store_core::rbac::write_action_from_str(a)
                    .map_err(napi::Error::from_reason)?;
                rust_store_core::rbac::ensure_write(&self.core, schema, context.as_ref(), wa)
                    .is_ok()
            }
        })
    }

    /// RBAC 叠加后的可读字段集（静态 ∩ readFields）；`ctx=null` → null 不裁剪
    #[napi]
    pub fn rbac_readable_fields(&self, model: String, ctx: Option<Value>) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        let base = get_readable_fields(schema, context.as_ref());
        Ok(sorted_set(rust_store_core::rbac::overlay_readable_fields(
            self.core.rbac(),
            &model,
            context.as_ref(),
            base,
        )))
    }

    /// RBAC 叠加后的可写字段集（静态 ∩ writeFields）；`ctx=null` → null 不裁剪
    #[napi]
    pub fn rbac_writable_fields(&self, model: String, ctx: Option<Value>) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        let base = get_writable_fields(schema, context.as_ref());
        Ok(sorted_set(rust_store_core::rbac::overlay_writable_fields(
            self.core.rbac(),
            &model,
            context.as_ref(),
            base,
        )))
    }

    /// RBAC 行级条件（ownerOnly / condition 的 OR 合并体）；null = 无行级收紧。
    /// `action ∈ {read, update, remove}`（insert 无行级语义）。
    #[napi]
    pub fn rbac_row_condition(
        &self,
        model: String,
        action: String,
        ctx: Option<Value>,
    ) -> Result<Value> {
        if !matches!(action.as_str(), "read" | "update" | "remove") {
            return Err(napi::Error::from_reason(format!(
                "RBAC row_condition 的 action \"{action}\" 非法（仅支持 read / update / remove）"
            )));
        }
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(rust_store_core::rbac::row_condition(
            &self.core,
            schema,
            context.as_ref(),
            &action,
        )
        .unwrap_or(Value::Null))
    }
}
