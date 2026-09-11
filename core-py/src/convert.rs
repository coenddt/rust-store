//! 绑定层通用转换：`serde_json::Value` ⇄ Python 对象 + 参数便捷封装。
//!
//! 与 core-node 的 `convert.rs` 逐函数对应（命名按各自语言习惯），只放与
//! core 语义无关的样板。

use std::collections::HashSet;

use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde_json::{Map, Value};

use rust_store_core::permission::{context_from_value, Context};

pub(crate) fn err(msg: String) -> PyErr {
    PyRuntimeError::new_err(msg)
}

/// `serde_json::Value` → Python 对象（None / bool / int / float / str / list / dict）
pub(crate) fn json_to_py<'py>(py: Python<'py>, v: &Value) -> PyResult<Bound<'py, PyAny>> {
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
pub(crate) fn py_to_json(v: &Bound<'_, PyAny>) -> PyResult<Value> {
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
pub(crate) fn to_py(py: Python<'_>, v: Value) -> PyResult<Py<PyAny>> {
    Ok(json_to_py(py, &v)?.unbind())
}

/// 权限上下文：`None` / Python `None` / dict → `Option<Context>`
pub(crate) fn ctx_from(v: Option<&Bound<'_, PyAny>>) -> PyResult<Option<Context>> {
    match v {
        None => Ok(None),
        Some(v) if v.is_none() => Ok(None),
        Some(v) => Ok(context_from_value(&py_to_json(v)?)),
    }
}

/// 查询参数：`None` / Python `None` / 非 dict → 空 map（对齐 napi 的 `params_map`）
pub(crate) fn params_from(v: Option<&Bound<'_, PyAny>>) -> PyResult<Map<String, Value>> {
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
pub(crate) fn sorted_set(set: Option<HashSet<String>>) -> Value {
    match set {
        None => Value::Null,
        Some(set) => {
            let mut v: Vec<String> = set.into_iter().collect();
            v.sort();
            Value::Array(v.into_iter().map(Value::String).collect())
        }
    }
}

/// Python list/tuple → `Vec<Value>`（非数组视为空列表）
pub(crate) fn value_list(v: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<Value>> {
    match v {
        Some(v) => match py_to_json(v)? {
            Value::Array(a) => Ok(a),
            _ => Ok(Vec::new()),
        },
        None => Ok(Vec::new()),
    }
}
