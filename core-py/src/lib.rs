//! mongo-store 的 Python 绑定（PyO3）
//!
//! 把 Rust core 的 Command 契约暴露给 Python Host：dict in / dict out，不执行任何 IO。
//!
//! 设计边界（对齐方案 A 的切分，与 `core-node` 完全同构）：
//! 1. core 不持有 MongoDB 驱动，本绑定层同样只产出「命令序列」与后处理结果；
//! 2. Python 函数无法经 `serde_json::Value` 传递（转换器只认基础类型），
//!    故**同步**计算列回调单独经 [`Registry::set_fn`] 注册，由 Rust 侧同步回调；
//! 3. **异步**计算列（`asyncFn`）无法被 Rust 同步等待，改由两段式承接：
//!    [`Registry::prepare_query`] 返回待执行 `fn_refs`（已做 read 权限过滤），
//!    Python Host 依次 `await` 后调 [`Registry::strip_query`]。
//!
//! 注意：错误必须经 `PyErr` **抛出**（`PyRuntimeError::new_err`），不可作为返回值
//! 混进 dict —— 否则 Python 侧 `try/except` 无法捕获（对齐 napi 侧「返回类型必须字面
//! 写 `Result`」的同类陷阱）。
//!
//! 文件组织：本文件只放「类定义 + 注册/回调注册等骨架方法」，其余方法按职责
//! 分块在 [`methods`]（同 struct 多个 `#[pymethods]` impl 块，依赖 PyO3 的
//! `multiple-pymethods` feature）；通用转换见 [`convert`]，回调适配见 [`fns`]。

use std::collections::HashMap;

use pyo3::prelude::*;

use rust_store_core::schema::Registry as CoreRegistry;

use crate::convert::{err, py_to_json};

mod convert;
mod fns;
mod methods;

// ─── 绑定入口 ───────────────────────────────────────────────

#[pyclass]
pub struct Registry {
    core: CoreRegistry,
    sync_fns: HashMap<String, Py<PyAny>>,
}

#[pymethods]
impl Registry {
    #[new]
    fn new() -> Self {
        Self {
            core: CoreRegistry::new(),
            sync_fns: HashMap::new(),
        }
    }

    /// 注册 schema（自动派生 `<Name>Deleted` 归档表；`timestamps` 非 false 时补时间戳字段）
    fn register(&mut self, defn: &Bound<'_, PyAny>) -> PyResult<()> {
        self.core.register(&py_to_json(defn)?).map_err(err)
    }

    fn has(&self, name: &str) -> bool {
        self.core.has(name)
    }

    fn list(&self) -> Vec<String> {
        self.core.list()
    }

    /// 注册同步计算列回调（schema 里 `fn: true` 的 `fnRef`，缺省为计算列名）
    fn set_fn(&mut self, fn_ref: String, callback: Py<PyAny>) {
        self.sync_fns.insert(fn_ref, callback);
    }

    fn clear_fns(&mut self) {
        self.sync_fns.clear();
    }

    /// 开关用户 $pipeline 直通（默认允许；AI 查询宿主建议关闭作纵深防御）
    fn set_allow_user_pipeline(&mut self, allow: bool) {
        self.core.set_allow_user_pipeline(allow);
    }

    /// 开关「上下文强制」（默认关闭 = fail-open，保持 JS parity）。
    /// 开启后：plan 入口遇 `ctx=None` 报 `ERR_NO_CONTEXT`（fail-secure），
    /// 内部调用须显式传系统上下文 `system_context()`。
    fn set_require_context(&mut self, require: bool) {
        self.core.set_require_context(require);
    }

    /// 「上下文强制」开关当前值
    fn require_context(&self) -> bool {
        self.core.require_context()
    }
}

/// 系统内部调用上下文工厂：`{"internal": true}` —— 权限引擎全放行、不注入 owner
/// 条件。供 Host 的内部路径（索引创建、归档回填、后台任务等）显式表达「系统调用」，
/// 与 `None`（未传上下文，`require_context` 开启时报错）区分。
#[pyfunction]
fn system_context(py: Python<'_>) -> PyResult<Bound<'_, pyo3::types::PyDict>> {
    let dict = pyo3::types::PyDict::new(py);
    dict.set_item("internal", true)?;
    Ok(dict)
}

#[pymodule]
fn rust_store_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Registry>()?;
    m.add_function(wrap_pyfunction!(system_context, m)?)?;
    Ok(())
}
