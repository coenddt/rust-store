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

use std::collections::{HashMap, HashSet};

use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde_json::{json, Map, Value};

use mongo_store_core::command::{
    plan_aggregate as core_plan_aggregate, plan_archive_docs as core_plan_archive_docs,
    plan_count as core_plan_count, plan_exists as core_plan_exists,
    plan_insert as core_plan_insert, plan_insert_many as core_plan_insert_many,
    plan_mutation as core_plan_mutation, plan_query as core_plan_query,
    plan_query_with_count as core_plan_query_with_count, plan_remove as core_plan_remove,
    plan_update as core_plan_update, plan_update_many as core_plan_update_many,
    plan_upsert as core_plan_upsert, prepare_query as core_prepare_query,
    resolve_page as core_resolve_page, restore_sort_order as core_restore_sort_order,
    sorts_by_relation as core_sorts_by_relation, strip_query as core_strip_query, Probe,
};
use mongo_store_core::computes::{
    apply_defaults_and_computes as core_apply_defaults, collect_rel_deps, merge_depends_into_ast,
    process_node, select_async_fns, strip_dep_injected as core_strip_dep_injected, FnRegistry,
    InjectInfo,
};
use mongo_store_core::dialect::{
    introspect_to_schema_json as core_introspect_to_schema_json,
    merge_schema as core_merge_schema, restore_rows_json as core_restore_rows_json,
    translate as core_dialect_translate, Backend,
};
use mongo_store_core::permission::{
    can_read_schema, can_write_schema, context_from_value, filter_writable_data,
    get_readable_fields, get_readable_relations, get_writable_fields, merge_owner_condition,
    should_inject_owner_condition, Context,
};
use mongo_store_core::pipeline::{
    build_pipeline, build_projection, parse_gql, token_to_value, tokenize,
};
use mongo_store_core::schema::Registry as CoreRegistry;

// ─── serde_json::Value ⇄ Python 对象 ────────────────────────

fn err(msg: String) -> PyErr {
    PyRuntimeError::new_err(msg)
}

/// `serde_json::Value` → Python 对象（None / bool / int / float / str / list / dict）
fn json_to_py<'py>(py: Python<'py>, v: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match v {
        Value::Null => py.None().into_bound(py),
        // `bool::into_pyobject` 产出 `Borrowed`（True/False 是单例），须先 `to_owned` 才能 `into_any`
        Value::Bool(b) => (*b).into_pyobject(py).unwrap().to_owned().into_any(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_pyobject(py).unwrap().into_any()
            } else if let Some(u) = n.as_u64() {
                u.into_pyobject(py).unwrap().into_any()
            } else {
                n.as_f64()
                    .unwrap_or(0.0)
                    .into_pyobject(py)
                    .unwrap()
                    .into_any()
            }
        }
        Value::String(s) => s.as_str().into_pyobject(py).unwrap().into_any(),
        Value::Array(a) => {
            let list = PyList::empty(py);
            for it in a {
                list.append(json_to_py(py, it)?)?;
            }
            list.into_any()
        }
        Value::Object(o) => {
            let dict = PyDict::new(py);
            for (k, val) in o {
                dict.set_item(k, json_to_py(py, val)?)?;
            }
            dict.into_any()
        }
    })
}

/// Python 对象 → `serde_json::Value`
///
/// `bool` 必须先于 `int` 判定（Python 的 `bool` 是 `int` 子类）。
fn py_to_json(v: &Bound<'_, PyAny>) -> PyResult<Value> {
    if v.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = v.cast::<PyBool>() {
        return Ok(Value::Bool(b.extract::<bool>()?));
    }
    if let Ok(i) = v.cast::<PyInt>() {
        if let Ok(n) = i.extract::<i64>() {
            return Ok(Value::Number(n.into()));
        }
        if let Ok(n) = i.extract::<u64>() {
            return Ok(Value::Number(n.into()));
        }
        if let Ok(n) = i.extract::<f64>() {
            return Ok(serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number));
        }
        return Err(PyTypeError::new_err("整数超出 serde_json 可表示范围"));
    }
    if let Ok(f) = v.cast::<PyFloat>() {
        let n: f64 = f.extract()?;
        return Ok(serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number));
    }
    if let Ok(s) = v.cast::<PyString>() {
        return Ok(Value::String(s.extract()?));
    }
    if v.cast::<PyList>().is_ok() || v.cast::<PyTuple>().is_ok() {
        let mut arr = Vec::new();
        for it in v.try_iter()? {
            arr.push(py_to_json(&it?)?);
        }
        return Ok(Value::Array(arr));
    }
    if let Ok(d) = v.cast::<PyDict>() {
        let mut map = Map::new();
        for (k, val) in d.iter() {
            let key: String = match k.cast::<PyString>() {
                Ok(s) => s.extract()?,
                Err(_) => k.str()?.extract()?,
            };
            map.insert(key, py_to_json(&val)?);
        }
        return Ok(Value::Object(map));
    }
    Err(PyTypeError::new_err(
        "不支持的 Python 类型（需为 None/bool/int/float/str/list/tuple/dict）",
    ))
}

/// 便捷封装：`Value` → `Py<PyAny>`
fn to_py(py: Python<'_>, v: Value) -> PyResult<Py<PyAny>> {
    Ok(json_to_py(py, &v)?.unbind())
}

/// 权限上下文：`None` / Python `None` / dict → `Option<Context>`
fn ctx_from(v: Option<&Bound<'_, PyAny>>) -> PyResult<Option<Context>> {
    match v {
        None => Ok(None),
        Some(v) if v.is_none() => Ok(None),
        Some(v) => Ok(context_from_value(&py_to_json(v)?)),
    }
}

/// 查询参数：`None` / Python `None` / 非 dict → 空 map（对齐 napi 的 `params_map`）
fn params_from(v: Option<&Bound<'_, PyAny>>) -> PyResult<Map<String, Value>> {
    match v {
        None => Ok(Map::new()),
        Some(v) if v.is_none() => Ok(Map::new()),
        Some(v) => match py_to_json(v)? {
            Value::Object(m) => Ok(m),
            _ => Ok(Map::new()),
        },
    }
}

/// `Option<HashSet<String>>` → `None` / 排序数组（保证跨语言比较稳定）
fn sorted_set(set: Option<HashSet<String>>) -> Value {
    match set {
        None => Value::Null,
        Some(set) => {
            let mut v: Vec<String> = set.into_iter().collect();
            v.sort();
            Value::Array(v.into_iter().map(Value::String).collect())
        }
    }
}

/// 探针状态换算：`probe_found` 缺省 = 未探查；`False` = 探针无结果（拒绝）；
/// `True` = 探针命中（取 `probe_doc`）
fn probe_of<'a>(probe_found: Option<bool>, probe_doc: Option<&'a Value>) -> Probe<'a> {
    match probe_found {
        None => Probe::NotProbed,
        Some(false) => Probe::NoResult,
        Some(true) => match probe_doc {
            Some(doc) => Probe::Found(doc),
            None => Probe::NoResult,
        },
    }
}

/// Python list/tuple → `Vec<Value>`（非数组视为空列表）
fn value_list(v: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<Value>> {
    match v {
        Some(v) => match py_to_json(v)? {
            Value::Array(a) => Ok(a),
            _ => Ok(Vec::new()),
        },
        None => Ok(Vec::new()),
    }
}

// ─── 回调桥 ─────────────────────────────────────────────────

/// 把 Python 注册的同步计算列回调适配成 core 的 [`FnRegistry`]
struct PyFnBridge<'a> {
    py: Python<'a>,
    fns: &'a HashMap<String, Py<PyAny>>,
}

impl FnRegistry for PyFnBridge<'_> {
    fn call_sync(&self, fn_ref: &str, doc: &Value) -> std::result::Result<Value, String> {
        let f = self
            .fns
            .get(fn_ref)
            .ok_or_else(|| format!("计算列 {} 未注册同步实现", fn_ref))?;
        let arg = json_to_py(self.py, doc).map_err(|e| e.to_string())?;
        let res = f.call1(self.py, (arg,)).map_err(|e| e.to_string())?;
        py_to_json(res.bind(self.py)).map_err(|e| e.to_string())
    }

    fn call_async(
        &self,
        fn_ref: &str,
        _items: &mut [Value],
        _ctx: Option<&Context>,
    ) -> std::result::Result<(), String> {
        // 异步回调由 Host 执行，core 内不会触发该分支
        Err(format!(
            "异步计算列 {} 需由 Host 执行（见 prepare_query 返回的 fn_refs）",
            fn_ref
        ))
    }
}

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

    // ─── Phase 1：GQL → pipeline / projection ───────────────

    /// 解析 GQL 并构建 pipeline / projection，返回 `{tokens, ast, pipeline, projection}`
    #[pyo3(signature = (gql, params=None, ctx=None))]
    fn build_pipeline(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let params = params_from(params)?;
        let context = ctx_from(ctx)?;

        let tokens = tokenize(&gql);
        let tokens_value = Value::Array(tokens.iter().map(token_to_value).collect());
        // `build_pipeline` 会原地展平 ast，故 ast 快照须在展平前取
        let mut ast = parse_gql(&gql).map_err(err)?;
        let ast_value = ast.to_value();
        let schema = self.core.get(&ast.model).map_err(err)?;
        let pipeline =
            build_pipeline(&mut ast, &params, &self.core, context.as_ref()).map_err(err)?;
        let projection =
            build_projection(&ast, schema, context.as_ref()).unwrap_or(Value::Null);

        to_py(
            py,
            json!({
                "tokens": tokens_value,
                "ast": ast_value,
                "pipeline": pipeline,
                "projection": projection,
            }),
        )
    }

    // ─── Phase 2：Command 序列 ─────────────────────────────

    #[pyo3(signature = (gql, params=None, ctx=None))]
    fn plan_query(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let params = params_from(params)?;
        let context = ctx_from(ctx)?;
        let plan = core_plan_query(&gql, &params, &self.core, context.as_ref()).map_err(err)?;
        to_py(py, plan.to_value())
    }

    /// 列表 + total；`total` 由 Host 执行 `countCommand` 后回喂，用于算 `hasMore`
    #[pyo3(signature = (gql, params=None, ctx=None, total=None))]
    fn plan_query_with_count(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        total: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        let params = params_from(params)?;
        let context = ctx_from(ctx)?;
        let plan =
            core_plan_query_with_count(&gql, &params, &self.core, context.as_ref()).map_err(err)?;

        let mut out = plan.to_value().as_object().cloned().unwrap_or_default();
        out.insert(
            "hasMore".to_string(),
            json!(plan.has_more(total.unwrap_or(0.0))),
        );
        to_py(py, Value::Object(out))
    }

    #[pyo3(signature = (gql, params=None))]
    fn resolve_page(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let ast = parse_gql(&gql).map_err(err)?;
        let params = params_from(params)?;
        to_py(py, core_resolve_page(&ast, &params).to_value())
    }

    /// 两阶段查询后按阶段一 `_id` 顺序重排，返回 `{items}`
    #[pyo3(signature = (items, ids, sort=None))]
    fn restore_sort_order(
        &self,
        py: Python<'_>,
        items: &Bound<'_, PyAny>,
        ids: &Bound<'_, PyAny>,
        sort: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let mut items = match py_to_json(items)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        let ids = match py_to_json(ids)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        let sort_v = match sort {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        core_restore_sort_order(&mut items, &ids, sort_v.as_ref());
        to_py(py, json!({ "items": items }))
    }

    /// 生成插入命令；`now` / `new_id` 由 Host 提供（core 无时钟与随机源）
    #[pyo3(signature = (model, data=None, now=0, new_id="", ctx=None))]
    fn plan_insert(
        &self,
        py: Python<'_>,
        model: String,
        data: Option<&Bound<'_, PyAny>>,
        now: i64,
        new_id: &str,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let context = ctx_from(ctx)?;
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let bridge = PyFnBridge {
            py,
            fns: &self.sync_fns,
        };
        let out = core_plan_insert(
            &model,
            &self.core,
            context.as_ref(),
            &data,
            now,
            new_id,
            Some(&bridge),
        )
        .map_err(err)?;
        to_py(py, out)
    }

    #[pyo3(signature = (model, condition=None))]
    fn plan_exists(
        &self,
        py: Python<'_>,
        model: String,
        condition: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let condition = match condition {
            Some(v) if !v.is_none() => py_to_json(v)?,
            _ => Value::Object(Map::new()),
        };
        let out = core_plan_exists(&model, &self.core, &condition).map_err(err)?;
        to_py(py, out)
    }

    #[pyo3(signature = (model, filter=None))]
    fn plan_count(
        &self,
        py: Python<'_>,
        model: String,
        filter: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let filter = match filter {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let out = core_plan_count(&model, &self.core, filter.as_ref()).map_err(err)?;
        to_py(py, out)
    }

    fn plan_aggregate(
        &self,
        py: Python<'_>,
        model: String,
        pipeline: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let pipeline = match py_to_json(pipeline)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        let out = core_plan_aggregate(&model, &self.core, &pipeline).map_err(err)?;
        to_py(py, out)
    }

    // ─── Phase 2.5：写路径命令规划 ─────────────────────────

    /// 批量插入命令；`new_ids` 按需消费（仅无 `_id` 的文档取用）
    #[pyo3(signature = (model, docs=None, now=0, new_ids=None, ctx=None))]
    fn plan_insert_many(
        &self,
        py: Python<'_>,
        model: String,
        docs: Option<&Bound<'_, PyAny>>,
        now: i64,
        new_ids: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let docs = value_list(docs)?;
        let new_ids: Vec<String> = value_list(new_ids)?
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            })
            .collect();
        let context = ctx_from(ctx)?;
        let bridge = PyFnBridge {
            py,
            fns: &self.sync_fns,
        };
        let out = core_plan_insert_many(
            &model,
            &self.core,
            context.as_ref(),
            &docs,
            now,
            &new_ids,
            Some(&bridge),
        )
        .map_err(err)?;
        to_py(py, out)
    }

    /// 更新一条（findOneAndUpdate + returnDocument AFTER）。
    ///
    /// creator 写权限需探针时返回 `{"needsProbe": cmd}`；Host 执行探针后携
    /// `probe_found`（True/False）与 `probe_doc` 重入即得 `{"command": cmd}`。
    #[pyo3(signature = (model, condition=None, data=None, options=None, now=0, ctx=None, probe_found=None, probe_doc=None))]
    fn plan_update(
        &self,
        py: Python<'_>,
        model: String,
        condition: Option<&Bound<'_, PyAny>>,
        data: Option<&Bound<'_, PyAny>>,
        options: Option<&Bound<'_, PyAny>>,
        now: i64,
        ctx: Option<&Bound<'_, PyAny>>,
        probe_found: Option<bool>,
        probe_doc: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let condition = match condition {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let options = match options {
            Some(v) if !v.is_none() => py_to_json(v)?,
            _ => Value::Null,
        };
        let probe_doc = match probe_doc {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let context = ctx_from(ctx)?;
        let out = core_plan_update(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            &data,
            &options,
            now,
            probe_of(probe_found, probe_doc.as_ref()),
        )
        .map_err(err)?;
        to_py(py, out)
    }

    /// 批量更新（guest / 无写授权直接拒绝，不走 creator 探针）
    #[pyo3(signature = (model, condition=None, data=None, now=0, ctx=None))]
    fn plan_update_many(
        &self,
        py: Python<'_>,
        model: String,
        condition: Option<&Bound<'_, PyAny>>,
        data: Option<&Bound<'_, PyAny>>,
        now: i64,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let condition = match condition {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let context = ctx_from(ctx)?;
        let out = core_plan_update_many(&model, &self.core, context.as_ref(), &condition, &data, now)
            .map_err(err)?;
        to_py(py, out)
    }

    /// 删除计划：归档表存在时返回 findCommand（Host 取源文档后调 planArchiveDocs）+
    /// deleteCommand。creator 探针语义同 planUpdate。
    #[pyo3(signature = (model, condition=None, ctx=None, probe_found=None, probe_doc=None))]
    fn plan_remove(
        &self,
        py: Python<'_>,
        model: String,
        condition: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        probe_found: Option<bool>,
        probe_doc: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let condition = match condition {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let probe_doc = match probe_doc {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let context = ctx_from(ctx)?;
        let out = core_plan_remove(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            probe_of(probe_found, probe_doc.as_ref()),
        )
        .map_err(err)?;
        to_py(py, out)
    }

    /// 归档文档命令：源文档补 `deletedAt` 后批量写入 `<collection>_deleted`
    #[pyo3(signature = (model, docs=None, now=0))]
    fn plan_archive_docs(
        &self,
        py: Python<'_>,
        model: String,
        docs: Option<&Bound<'_, PyAny>>,
        now: i64,
    ) -> PyResult<Py<PyAny>> {
        let docs = value_list(docs)?;
        let out = core_plan_archive_docs(&model, &self.core, &docs, now).map_err(err)?;
        to_py(py, out)
    }

    /// 显式条件 upsert；`new_id` 仅在需生成 `_id` 时被使用
    #[pyo3(signature = (model, condition=None, data=None, options=None, now=0, new_id="", ctx=None))]
    fn plan_upsert(
        &self,
        py: Python<'_>,
        model: String,
        condition: Option<&Bound<'_, PyAny>>,
        data: Option<&Bound<'_, PyAny>>,
        options: Option<&Bound<'_, PyAny>>,
        now: i64,
        new_id: &str,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let condition = match condition {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let options = match options {
            Some(v) if !v.is_none() => py_to_json(v)?,
            _ => Value::Null,
        };
        let context = ctx_from(ctx)?;
        let out = core_plan_upsert(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            &data,
            &options,
            now,
            new_id,
        )
        .map_err(err)?;
        to_py(py, out)
    }

    /// mutation 规划：展开为有序步骤序列 `{steps: [{model, command}]}`，
    /// 父子依赖用 `{{step.<N>._id}}` 占位符表达，由 Host 依次执行并回填
    #[pyo3(signature = (model, data=None, now=0, new_ids=None, ctx=None))]
    fn plan_mutation(
        &self,
        py: Python<'_>,
        model: String,
        data: Option<&Bound<'_, PyAny>>,
        now: i64,
        new_ids: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let new_ids: Vec<String> = value_list(new_ids)?
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            })
            .collect();
        let context = ctx_from(ctx)?;
        let out =
            core_plan_mutation(&model, &self.core, context.as_ref(), &data, now, &new_ids)
                .map_err(err)?;
        to_py(py, out)
    }

    /// 写路径结果回喂：对 findOneAndUpdate 返回文档补默认值 / 同步计算列
    /// （对齐 JS `applyDefaultsAndComputes(result, s)`）
    fn apply_write_defaults(
        &self,
        py: Python<'_>,
        model: String,
        doc: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let doc = py_to_json(doc)?;
        let bridge = PyFnBridge {
            py,
            fns: &self.sync_fns,
        };
        let out = core_apply_defaults(&doc, schema, Some(&bridge)).map_err(err)?;
        to_py(py, out)
    }

    #[pyo3(signature = (sort=None))]
    fn sorts_by_relation(&self, sort: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let sort_v = match sort {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        Ok(core_sorts_by_relation(sort_v.as_ref()))
    }

    // ─── Phase 3：计算列 / 依赖注入 / 结果回喂 ─────────────

    /// 逐条后处理文档（默认值 → 同步 fn → 递归下钻 → 权限裁剪），返回 `{doc}`
    #[pyo3(signature = (gql, doc, ctx=None))]
    fn process_node(
        &self,
        py: Python<'_>,
        gql: String,
        doc: &Bound<'_, PyAny>,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let mut ast = parse_gql(&gql).map_err(err)?;
        let schema = self.core.get(&ast.model).map_err(err)?;
        let context = ctx_from(ctx)?;
        let bridge = PyFnBridge {
            py,
            fns: &self.sync_fns,
        };
        let mut doc = py_to_json(doc)?;

        process_node(
            &mut doc,
            &mut ast.fields,
            &mut ast.relations,
            schema,
            context.as_ref(),
            &self.core,
            Some(&bridge),
        )
        .map_err(err)?;
        to_py(py, json!({ "doc": doc }))
    }

    /// 需由 Host 异步执行的计算列 `fn_ref` 列表（已按 `comp.read` 权限过滤）
    #[pyo3(signature = (model, ctx=None))]
    fn async_fn_refs(
        &self,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Vec<String>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(select_async_fns(schema, context.as_ref())
            .into_iter()
            .map(|e| e.fn_ref)
            .collect())
    }

    /// 收集关系依赖并注入 AST，返回 `{relDeps, ast, injectInfo}`
    fn inject_depends(&self, py: Python<'_>, gql: String) -> PyResult<Py<PyAny>> {
        let mut ast = parse_gql(&gql).map_err(err)?;
        let schema = self.core.get(&ast.model).map_err(err)?;

        let deps = collect_rel_deps(schema).map_err(err)?;
        let deps_value = Value::Array(deps.iter().map(|d| d.to_value()).collect());
        let info = merge_depends_into_ast(&mut ast.relations, schema).map_err(err)?;

        to_py(
            py,
            json!({
                "relDeps": deps_value,
                "ast": ast.to_value(),
                "injectInfo": info.to_value(),
            }),
        )
    }

    /// 剥离 asyncFn 依赖注入的字段，返回 `{items}`
    fn strip_dep_injected(
        &self,
        py: Python<'_>,
        inject_info: &Bound<'_, PyAny>,
        items: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let info = InjectInfo::from_value(&py_to_json(inject_info)?);
        let mut items = match py_to_json(items)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        core_strip_dep_injected(&mut items, &info);
        to_py(py, json!({ "items": items }))
    }

    /// finalize 第一阶段：逐条 `process_node`，返回 `{items, fnRefs}`
    #[pyo3(signature = (postprocess, items, ctx=None))]
    fn prepare_query(
        &self,
        py: Python<'_>,
        postprocess: &Bound<'_, PyAny>,
        items: &Bound<'_, PyAny>,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let context = ctx_from(ctx)?;
        let bridge = PyFnBridge {
            py,
            fns: &self.sync_fns,
        };
        let mut items = match py_to_json(items)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        let postprocess = py_to_json(postprocess)?;

        let fn_refs = core_prepare_query(
            &postprocess,
            &mut items,
            &self.core,
            Some(&bridge),
            context.as_ref(),
        )
        .map_err(err)?;

        to_py(py, json!({ "items": items, "fnRefs": fn_refs }))
    }

    /// finalize 第三阶段：剥离 asyncFn 依赖注入的字段，返回 `{items}`
    fn strip_query(
        &self,
        py: Python<'_>,
        postprocess: &Bound<'_, PyAny>,
        items: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let postprocess = py_to_json(postprocess)?;
        let mut items = match py_to_json(items)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        core_strip_query(&postprocess, &mut items);
        to_py(py, json!({ "items": items }))
    }

    // ─── 权限 ─────────────────────────────────────────────

    #[pyo3(signature = (model, ctx=None))]
    fn can_read(&self, model: String, ctx: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(can_read_schema(schema, context.as_ref()))
    }

    #[pyo3(signature = (model, ctx=None))]
    fn can_write(&self, model: String, ctx: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(can_write_schema(schema, context.as_ref()))
    }

    #[pyo3(signature = (model, ctx=None))]
    fn should_inject_owner(
        &self,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(should_inject_owner_condition(schema, context.as_ref()))
    }

    /// 非 admin 用户只看自己数据时叠加 owner 条件；无上下文时原样返回
    #[pyo3(signature = (model, ctx=None, condition=None))]
    fn merge_owner_condition(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
        condition: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        let condition = match condition {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let out =
            merge_owner_condition(schema, context.as_ref(), condition).unwrap_or(Value::Null);
        to_py(py, out)
    }

    #[pyo3(signature = (model, ctx=None))]
    fn readable_fields(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        to_py(py, sorted_set(get_readable_fields(schema, context.as_ref())))
    }

    #[pyo3(signature = (model, ctx=None))]
    fn readable_relations(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        to_py(py, sorted_set(get_readable_relations(schema, context.as_ref())))
    }

    #[pyo3(signature = (model, ctx=None))]
    fn writable_fields(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        to_py(py, sorted_set(get_writable_fields(schema, context.as_ref())))
    }

    #[pyo3(signature = (model, ctx=None, data=None))]
    fn filter_writable_data(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
        data: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        to_py(py, filter_writable_data(schema, context.as_ref(), &data))
    }

    // ─── dialect：Mongo 命令 → 关系型 SQL ─────────────

    #[pyo3(signature = (backend, cmd))]
    fn dialect_translate(
        &self,
        py: Python<'_>,
        backend: String,
        cmd: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let backend = Backend::parse(&backend).map_err(err)?;
        let cmd = match cmd {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        to_py(py, core_dialect_translate(backend, &cmd, &self.core).map_err(err)?)
    }

    #[pyo3(signature = (shape, rows))]
    fn restore_rows(
        &self,
        py: Python<'_>,
        shape: Option<&Bound<'_, PyAny>>,
        rows: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let shape = match shape {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let rows = match rows {
            Some(v) => py_to_json(v)?,
            None => Value::Array(Vec::new()),
        };
        to_py(py, core_restore_rows_json(&shape, &rows).map_err(err)?)
    }

    #[pyo3(signature = (rows, backend))]
    fn schema_from_rows(
        &self,
        py: Python<'_>,
        rows: Option<&Bound<'_, PyAny>>,
        backend: String,
    ) -> PyResult<Py<PyAny>> {
        let backend = Backend::parse(&backend).map_err(err)?;
        let rows = match rows {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        to_py(py, core_introspect_to_schema_json(&rows, &backend).map_err(err)?)
    }

    #[pyo3(signature = (base, overlay))]
    fn merge_schema(
        &self,
        py: Python<'_>,
        base: Option<&Bound<'_, PyAny>>,
        overlay: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let base = match base {
            Some(v) => py_to_json(v)?,
            None => Value::Array(Vec::new()),
        };
        let overlay = match overlay {
            Some(v) => py_to_json(v)?,
            None => Value::Array(Vec::new()),
        };
        to_py(py, core_merge_schema(&base, &overlay).map_err(err)?)
    }
}

#[pymodule]
fn mongo_store_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Registry>()?;
    Ok(())
}
