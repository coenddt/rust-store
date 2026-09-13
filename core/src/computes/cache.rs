//! schema 维度缓存：字段默认值 / 计算列零值 / fn 与 asyncFn 声明列表

use serde_json::{Map, Value};

use crate::schema::Schema;
use crate::types::get_default;

/// fn / asyncFn 计算列的声明信息（fn_ref 绑定 Host 实现）
#[derive(Debug, Clone)]
pub struct ComputeEntry {
    pub key: String,
    /// 回调标识（`fnRef`，缺省 = key 名）
    pub fn_ref: String,
    pub depends: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Cache {
    /// 有默认值的字段：`key → 默认值`
    pub field_defaults: Map<String, Value>,
    /// 计算列按 `type` 推出的零值：`key → 零值`
    pub compute_defaults: Map<String, Value>,
    pub fn_list: Vec<ComputeEntry>,
    pub async_fn_list: Vec<ComputeEntry>,
}

/// 构建 schema 维度的默认值 / 计算列缓存（对应 JS `_ensureCache`）
pub fn ensure_cache(schema: &Schema) -> Cache {
    let mut field_defaults = Map::new();
    for (key, field) in &schema.fields {
        // read 后处理 `fill_defaults` 只应补「schema 显式 `default`」的字段；
        // 不按类型派生零值注入，否则存储 null/缺失 的标量被覆盖为 ""/0
        // （H-01：summary/categoryId 无显式 default，null→null、缺失→缺失）。
        // 写入路径的 `apply_defaults_and_computes` 仍走 `field_default` 保留类型默认。
        if let Some(d) = field.default.as_ref() {
            if !d.is_null() {
                field_defaults.insert(key.clone(), d.clone());
            }
        }
    }

    let mut compute_defaults = Map::new();
    let mut fn_list = Vec::new();
    let mut async_fn_list = Vec::new();
    for (key, comp) in &schema.computes {
        compute_defaults.insert(key.clone(), get_default(&comp.comp_type));
        let fn_ref = comp.fn_ref.clone().unwrap_or_else(|| key.clone());
        if comp.has_fn {
            fn_list.push(ComputeEntry {
                key: key.clone(),
                fn_ref: fn_ref.clone(),
                depends: comp.depends.clone(),
            });
        }
        if comp.has_async_fn {
            async_fn_list.push(ComputeEntry {
                key: key.clone(),
                fn_ref,
                depends: comp.depends.clone(),
            });
        }
    }

    Cache {
        field_defaults,
        compute_defaults,
        fn_list,
        async_fn_list,
    }
}
