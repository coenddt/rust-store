//! 权限（读 / 写 / 字段裁剪 / owner 条件）方法。

use napi::Result;
use napi_derive::napi;
use serde_json::Value;

use rust_store_core::permission::{
    can_read_schema, can_write_schema, context_from_value, filter_writable_data,
    get_readable_fields, get_readable_relations, get_writable_fields, merge_owner_condition,
    should_inject_owner_condition,
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
}
