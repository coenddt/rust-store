//! JS 同步计算列回调 → core [`FnRegistry`] 适配。
//!
//! 异步计算列（`asyncFn`）无法被 Rust 同步等待，本适配器在该分支直接报错，
//! 由 Host 走 `prepareQuery` / `stripQuery` 两段式承接。

use std::collections::HashMap;

use napi::bindgen_prelude::{Env, FunctionRef};
use serde_json::Value;

use rust_store_core::computes::FnRegistry;
use rust_store_core::permission::Context;

pub(crate) struct SyncFnBridge<'a> {
    pub(crate) env: &'a Env,
    pub(crate) fns: &'a HashMap<String, FunctionRef<Value, Value>>,
}

impl FnRegistry for SyncFnBridge<'_> {
    fn call_sync(&self, fn_ref: &str, doc: &Value) -> std::result::Result<Value, String> {
        let f = self
            .fns
            .get(fn_ref)
            .ok_or_else(|| format!("计算列 {} 未注册同步实现", fn_ref))?;
        let func = f.borrow_back(self.env).map_err(|e| e.to_string())?;
        func.call(doc.clone()).map_err(|e| e.to_string())
    }

    fn call_async(
        &self,
        fn_ref: &str,
        _items: &mut [Value],
        _ctx: Option<&Context>,
    ) -> std::result::Result<(), String> {
        // 异步回调由 Host 执行，core 内不会触发该分支
        Err(format!(
            "异步计算列 {} 需由 Host 执行（见 prepareQuery 返回的 fnRefs）",
            fn_ref
        ))
    }
}
