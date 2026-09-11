//! fn / asyncFn 回调桥

use serde_json::Value;

use crate::permission::Context;

/// fn / asyncFn 执行体注册表（Host 侧实现；napi/PyO3 绑定桥接到原生闭包）
pub trait FnRegistry {
    /// 同步 fn 计算列：`doc[key] = fn(doc)`
    fn call_sync(&self, fn_ref: &str, doc: &Value) -> Result<Value, String>;
    /// asyncFn 计算列：批量原地改写 items（对应 JS `await asyncFn(items, ctx)`）
    fn call_async(
        &self,
        fn_ref: &str,
        items: &mut [Value],
        ctx: Option<&Context>,
    ) -> Result<(), String>;
}
