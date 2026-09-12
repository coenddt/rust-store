//! rust-store 的 Node 绑定（napi-rs）
//!
//! 把 Rust core 的 Command 契约暴露给 JS Host：JSON in / JSON out，不执行任何 IO。
//!
//! 设计边界（对齐方案 A 的切分）：
//! 1. core 不持有 MongoDB 驱动，本绑定层同样只产出「命令序列」与后处理结果；
//! 2. JS 函数无法经 `serde_json::Value` 传递（napi 遇到 Function 会报 `InvalidArg`），
//!    故**同步**计算列回调单独经 [`Registry::set_fn`] 注册，由 Rust 侧同步回调；
//! 3. **异步**计算列（`asyncFn`）无法被 Rust 同步等待，改由两段式承接：
//!    [`Registry::prepare_query`] 返回待执行 `fnRefs`（已做 read 权限过滤），
//!    JS Host 依次 `await` 后调 [`Registry::strip_query`]。
//!
//! 注意：返回类型**必须**写成 `napi::Result<T>`（不能用类型别名）。napi-derive 靠
//! 「路径末段 ident == `Result`」判定是否生成抛异常代码（见 napi-derive
//! `parser/mod.rs::extract_result_ty`）；一旦改成别名，`Err` 会被当作普通返回值
//! 经 `impl ToNapiValue for Error` 序列化成一个 `{code}` 对象返回给 JS，而非抛出。
//!
//! 文件组织：本文件只放「类定义 + 注册/回调注册等骨架方法」，其余方法按职责
//! 分块在 [`methods`]（同 struct 多个 `#[napi] impl` 块）；通用转换见 [`convert`]，
//! 回调适配见 [`fns`]。

use std::collections::HashMap;

use napi::bindgen_prelude::{Env, FunctionRef};
use napi::Result;
use napi_derive::napi;
use serde_json::{Map, Value};

use rust_store_core::schema::Registry as CoreRegistry;

use crate::fns::SyncFnBridge;

mod convert;
mod fns;
mod methods;

#[napi]
pub struct Registry {
    core: CoreRegistry,
    sync_fns: HashMap<String, FunctionRef<Value, Value>>,
}

impl Registry {
    pub(crate) fn bridge<'a>(&'a self, env: &'a Env) -> SyncFnBridge<'a> {
        SyncFnBridge {
            env,
            fns: &self.sync_fns,
        }
    }

    pub(crate) fn params_map(params: &Value) -> Map<String, Value> {
        params.as_object().cloned().unwrap_or_default()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[napi]
impl Registry {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            core: CoreRegistry::new(),
            sync_fns: HashMap::new(),
        }
    }

    /// 注册 schema（自动派生 `<Name>Deleted` 归档表；`timestamps !== false` 时补时间戳字段）
    #[napi]
    pub fn register(&mut self, defn: Value) -> Result<()> {
        self.core.register(&defn).map_err(convert::err)
    }

    #[napi]
    pub fn has(&self, name: String) -> bool {
        self.core.has(&name)
    }

    #[napi]
    pub fn list(&self) -> Vec<String> {
        self.core.list()
    }

    /// 注册同步计算列回调（schema 里 `fn: true` 的 `fnRef`，缺省为计算列名）
    #[napi]
    pub fn set_fn(&mut self, fn_ref: String, callback: FunctionRef<Value, Value>) {
        self.sync_fns.insert(fn_ref, callback);
    }

    #[napi]
    pub fn clear_fns(&mut self) {
        self.sync_fns.clear();
    }

    /// 开关用户 $pipeline 直通（默认允许；AI 查询宿主建议关闭作纵深防御）
    #[napi]
    pub fn set_allow_user_pipeline(&mut self, allow: bool) {
        self.core.set_allow_user_pipeline(allow);
    }

    /// 开关「上下文强制」（默认关闭 = fail-open，保持 JS parity）。
    /// 开启后：plan 入口遇 `ctx` 缺失抛 `ERR_NO_CONTEXT`（fail-secure），
    /// 内部调用须显式传系统上下文 `systemContext()`。
    #[napi]
    pub fn set_require_context(&mut self, require: bool) {
        self.core.set_require_context(require);
    }

    /// 「上下文强制」开关当前值
    #[napi]
    pub fn require_context(&self) -> bool {
        self.core.require_context()
    }
}

/// 系统内部调用上下文工厂：`{ internal: true }` —— 权限引擎全放行、不注入 owner
/// 条件。供 Host 的内部路径（索引创建、归档回填、后台任务等）显式表达「系统调用」，
/// 与 `undefined`（未传上下文，`require_context` 开启时报错）区分。
#[napi]
pub fn system_context() -> Value {
    serde_json::json!({ "internal": true })
}
