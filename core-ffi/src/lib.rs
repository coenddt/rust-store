//! rust-store-ffi —— go-store 用的 C ABI 绑定。
//!
//! 边界约定（与 core-py / core-node 的分工对齐）：
//!   - core 承载全部纯逻辑（GQL 解析 / 权限 / 命令规划 / SQL 方言翻译 / 计算列规划）；
//!   - 本 crate 只做三件事：Registry 句柄管理、`plan_*` / `translate` / 后处理两段式的
//!     C ABI 转发、FnRegistry 回调桥（把 fn/asyncFn 执行体转交 Go 侧闭包）；
//!   - 数据边界一律 UTF-8 JSON 字符串（core 出入参本就是 `serde_json::Value`）；
//!   - 每个导出的返回值统一信封 `{"ok":true,"data":...}` / `{"ok":false,"error":"..."}`，
//!     由调用方（go-store）用 `rcore_free` 释放；
//!   - 后处理走 core 钦定的 FFI 两段式（finalize.rs 文档）：
//!     `prepare_query`（同步 fn 内联、返回待宿主执行的 asyncFn fnRef 列表）
//!     → Host 执行 asyncFn → `strip_query`（剥离依赖注入字段）。
//!
//! 铁律（no-error-masking）：任何失败都以 `{"ok":false,"error":...}` 显式浮出，
//! 绝不返回残缺数据；panics 也被 catch_unwind 捕获后转显式错误信封。

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rust_store_core::command::{
    finalize_query, plan_archive_docs, plan_insert, plan_query, plan_query_one,
    plan_remove, plan_update, restore_sort_order, Probe, PHASE1_IDS,
};
use rust_store_core::computes::FnRegistry;
use rust_store_core::dialect::{restore_rows_json, translate, Backend};
use rust_store_core::permission::Context;
use rust_store_core::schema::Registry;
use serde_json::{json, Map, Value};

/// 两阶段查询占位符（core `PHASE1_IDS` 的字面量，供宿主按字符串替换）
pub const PHASE1_IDS_STR: &str = PHASE1_IDS;

// ── Registry 句柄表 ──

static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(1);
static REGISTRIES: Mutex<Option<HashMap<usize, Registry>>> = Mutex::new(None);

fn with_registries<T>(f: impl FnOnce(&mut HashMap<usize, Registry>) -> Result<T, String>) -> Result<T, String> {
    let mut guard = REGISTRIES.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

// ── Go 计算列回调桥 ──

/// Go 侧回调：`fn(fn_ref: *const c_char, kind: c_int, payload: *const c_char) -> *mut c_char`
///   - kind 0：同步 fn（payload = `{"doc":...,"ctx":...}`，返回 `{"value":...}` 或 `{"error":"..."}`）
///   - kind 1：批量 asyncFn（payload = `{"docs":[...],"ctx":...}`，返回 `{"docs":[...]}` 或 `{"error":"..."}`）
///   - 返回值：Go 侧分配的 NUL 结尾 UTF-8 缓冲区首指针；Rust 复制完立即调用 free_fp 释放。
type GoComputeFn = extern "C" fn(*const c_char, c_int, *const c_char) -> *mut c_char;
type GoFreeFn = extern "C" fn(*mut c_char);

static COMPUTE_CB: AtomicUsize = AtomicUsize::new(0);
static COMPUTE_FREE: AtomicUsize = AtomicUsize::new(0);

/// FnRegistry 的跨 FFI 实现：把 core 的回调桥转发给 Go 注册的函数指针。
/// 携带 Registry 句柄，让 Go 侧能把回调路由回对应的 Store 实例。
struct BridgeFnRegistry {
    handle: u64,
}

fn go_payload_ptr(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| "Go 计算列回调入参含 NUL 字节".to_string())
}

fn call_go_compute(kind: c_int, fn_ref: &str, payload: &Value) -> Result<Value, String> {
    let fp = COMPUTE_CB.load(Ordering::Acquire);
    let free_fp = COMPUTE_FREE.load(Ordering::Acquire);
    if fp == 0 {
        return Err(format!(
            "计算列 {fn_ref} 需要 Go 回调但回调指针未注册（应先调用 rcore_set_compute_callback）"
        ));
    }
    let cb: GoComputeFn = unsafe { std::mem::transmute(fp as *mut ()) };
    let c_ref = go_payload_ptr(fn_ref)?;
    let c_payload = go_payload_ptr(&payload.to_string())?;
    let ret = cb(c_ref.as_ptr(), kind, c_payload.as_ptr());
    if ret.is_null() {
        return Err(format!("计算列 {fn_ref} 的 Go 回调返回空指针"));
    }
    let s = unsafe { CStr::from_ptr(ret) }.to_string_lossy().into_owned();
    if free_fp != 0 {
        let free: GoFreeFn = unsafe { std::mem::transmute(free_fp as *mut ()) };
        free(ret);
    }
    let v: Value = serde_json::from_str(&s)
        .map_err(|e| format!("计算列 {fn_ref} 的 Go 回调返回非法 JSON: {e}"))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(format!("计算列 {fn_ref} 执行失败: {err}"));
    }
    Ok(v)
}

impl FnRegistry for BridgeFnRegistry {
    fn call_sync(&self, fn_ref: &str, doc: &Value) -> Result<Value, String> {
        let payload = json!({ "handle": self.handle, "doc": doc, "ctx": Value::Null });
        let out = call_go_compute(0, fn_ref, &payload)?;
        out.get("value").cloned().ok_or_else(|| {
            format!("计算列 {fn_ref} 的 Go 回调返回缺少 value 字段")
        })
    }

    fn call_async(
        &self,
        fn_ref: &str,
        items: &mut [Value],
        ctx: Option<&Context>,
    ) -> Result<(), String> {
        let payload = json!({ "handle": self.handle, "docs": items, "ctx": ctx_to_value(ctx) });
        let out = call_go_compute(1, fn_ref, &payload)?;
        let docs = out.get("docs").and_then(|d| d.as_array()).ok_or_else(|| {
            format!("计算列 {fn_ref} 的 Go 回调返回缺少 docs 数组")
        })?;
        if docs.len() != items.len() {
            return Err(format!(
                "计算列 {fn_ref} 的 Go 回调返回 docs 数量（{}）与输入（{}）不一致",
                docs.len(),
                items.len()
            ));
        }
        for (slot, doc) in items.iter_mut().zip(docs) {
            *slot = doc.clone();
        }
        Ok(())
    }
}

fn as_trait(o: Option<&BridgeFnRegistry>) -> Option<&dyn FnRegistry> {
    o.map(|b| b as &dyn FnRegistry)
}

fn bridge(handle: u64) -> Option<BridgeFnRegistry> {
    if COMPUTE_CB.load(Ordering::Acquire) == 0 {
        None
    } else {
        Some(BridgeFnRegistry { handle })
    }
}

// ── JSON ↔ core 类型转换 ──

fn ctx_to_value(ctx: Option<&Context>) -> Value {
    match ctx {
        None => Value::Null,
        Some(c) => json!({
            "userId": c.user_id,
            "roles": c.roles,
            "role": c.role,
            "internal": c.internal,
        }),
    }
}

fn value_to_ctx(v: &Value) -> Result<Option<Context>, String> {
    if v.is_null() {
        return Ok(None);
    }
    let obj = v
        .as_object()
        .ok_or_else(|| "ctx 必须是对象或 null".to_string())?;
    let user_id = obj
        .get("userId")
        .and_then(|x| x.as_str().map(String::from));
    let roles = match obj.get("roles") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => Some(
            a.iter()
                .map(|x| {
                    x.as_str()
                        .map(String::from)
                        .ok_or_else(|| "ctx.roles 元素必须是字符串".to_string())
                })
                .collect::<Result<Vec<String>, String>>()?,
        ),
        Some(_) => return Err("ctx.roles 必须是数组或 null".to_string()),
    };
    let role = obj
        .get("role")
        .and_then(|x| x.as_str().map(String::from));
    let internal = obj
        .get("internal")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    Ok(Some(Context {
        user_id,
        roles,
        role,
        internal,
    }))
}

/// probe 编码：`"not_probed"` | `"no_result"` | `{"found": {...}}`
fn value_to_probe(v: &Value) -> Result<Probe<'_>, String> {
    match v {
        Value::String(s) if s == "not_probed" => Ok(Probe::NotProbed),
        Value::String(s) if s == "no_result" => Ok(Probe::NoResult),
        Value::Object(_) => {
            let found = v
                .get("found")
                .ok_or_else(|| "probe 对象形态必须含 found 字段".to_string())?;
            Ok(Probe::Found(found))
        }
        _ => Err("probe 必须是 \"not_probed\" / \"no_result\" 或 {\"found\":{...}}".to_string()),
    }
}

fn parse_params(v: &Value) -> Result<Map<String, Value>, String> {
    v.as_object()
        .cloned()
        .ok_or_else(|| "params 必须是对象".to_string())
}

// ── C 边界内存工具 ──

/// 把结果装箱为 NUL 结尾字符串返回；null 仅在信封序列化失败时出现（go 侧视为致命）。
fn ret_ok(data: Value) -> *mut c_char {
    ret_envelope(&json!({ "ok": true, "data": data }))
}

fn ret_err(msg: String) -> *mut c_char {
    ret_envelope(&json!({ "ok": false, "error": msg }))
}

fn ret_envelope(v: &Value) -> *mut c_char {
    // CString::new 只在含 NUL 时失败；JSON 字符串序列化后不含字面 NUL，unwrap 前已兜底为错误文本
    match CString::new(v.to_string()) {
        Ok(c) => c.into_raw(),
        Err(_) => CString::new("{\"ok\":false,\"error\":\"FFI 信封序列化失败\"}")
            .expect("固定错误文本不含 NUL")
            .into_raw(),
    }
}

/// 宿主入口统一包装：panic → 显式错误信封（禁静默失守）。
fn guard(f: impl FnOnce() -> Result<Value, String>) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => ret_ok(v),
        Ok(Err(e)) => ret_err(e),
        Err(p) => {
            let msg = if let Some(s) = p.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = p.downcast_ref::<String>() {
                s.clone()
            } else {
                "core 内部 panic（已捕获，无数据返回）".to_string()
            };
            ret_err(msg)
        }
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> Result<&'a str, String> {
    if p.is_null() {
        return Err("入参字符串指针为 null".to_string());
    }
    CStr::from_ptr(p)
        .to_str()
        .map_err(|_| "入参不是合法 UTF-8".to_string())
}

unsafe fn json_arg(p: *const c_char) -> Result<Value, String> {
    let s = cstr(p)?;
    serde_json::from_str(s).map_err(|e| format!("入参 JSON 解析失败: {e}"))
}

// ── 导出面 ──

#[no_mangle]
pub extern "C" fn rcore_api_version() -> *const c_char {
    static V: &[u8] = b"go-store ffi v0.1.0 / core contract 2026-09\0";
    V.as_ptr() as *const c_char
}

/// 释放本 crate 返回的字符串（调用方义务）
#[no_mangle]
pub extern "C" fn rcore_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe { drop(CString::from_raw(ptr)) };
    }
}

/// 注册 Go 计算列回调与配对释放函数（free_fp 用于释放回调返回的 Go 缓冲区）
#[no_mangle]
pub extern "C" fn rcore_set_compute_callback(fp: usize, free_fp: usize) -> c_int {
    COMPUTE_CB.store(fp, Ordering::Release);
    COMPUTE_FREE.store(free_fp, Ordering::Release);
    0
}

/// 新建 Registry 句柄；require_context 非 0 表示「必须带上下文」（默认关闭 = fail-open，与双宿主一致）
#[no_mangle]
pub extern "C" fn rcore_registry_new(require_context: c_int) -> u64 {
    let mut reg = Registry::new();
    if require_context != 0 {
        reg.set_require_context(true);
    }
    let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed) as u64;
    let res = with_registries(|map| {
        map.insert(h as usize, reg);
        Ok(())
    });
    match res {
        Ok(()) => h,
        // 句柄 0 保留为错误哨兵
        Err(_) => 0,
    }
}

/// 释放句柄（幂等；未知名句柄返回 ok）
#[no_mangle]
pub extern "C" fn rcore_registry_drop(handle: u64) {
    let _ = with_registries(|map| {
        map.remove(&(handle as usize));
        Ok(())
    });
}

#[no_mangle]
pub unsafe extern "C" fn rcore_registry_register(handle: u64, defn: *const c_char) -> *mut c_char {
    guard(|| {
        let defn = unsafe { json_arg(defn) }?;
        with_registries(|map| {
            let reg = map
                .get_mut(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            reg.register(&defn)
        })?;
        Ok(json!(null))
    })
}

#[no_mangle]
pub unsafe extern "C" fn rcore_registry_list(handle: u64) -> *mut c_char {
    guard(|| {
        let list = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            Ok(reg.list())
        })?;
        Ok(json!(list))
    })
}

// ── RBAC 配置注入与查询面（判决唯一在 core；本层零判决逻辑） ──

/// 注入/清除 RBAC 策略：`policy` 为 null = 清除；否则为策略 JSON。
/// 解析失败以 `{"ok":false,"error":...}` 显式浮出（fail-fast）。
#[no_mangle]
pub unsafe extern "C" fn rcore_registry_set_rbac(handle: u64, policy: *const c_char) -> *mut c_char {
    guard(|| {
        let policy = unsafe { json_arg(policy) }?;
        // JSON "null" / 缺失 → None（清除）；否则 Some
        let policy_opt = if policy.is_null() { None } else { Some(&policy) };
        with_registries(|map| {
            let reg = map
                .get_mut(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            reg.set_rbac(policy_opt)
        })?;
        Ok(json!(null))
    })
}

/// RBAC 策略是否已注入（data: true/false）
#[no_mangle]
pub unsafe extern "C" fn rcore_rbac_enabled(handle: u64) -> *mut c_char {
    guard(|| {
        let enabled = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            Ok(reg.rbac().is_some())
        })?;
        Ok(json!(enabled))
    })
}

/// RBAC 动作判决：`action ∈ {read, insert, update, remove}`（data: true/false）。
/// 策略未注入 / RBAC 不介入 → true（与 plan 链路的实际拦截结果一致）。
#[no_mangle]
pub unsafe extern "C" fn rcore_rbac_can(
    handle: u64,
    model: *const c_char,
    action: *const c_char,
    ctx: *const c_char,
) -> *mut c_char {
    guard(|| {
        let model = unsafe { cstr(model) }?.to_string();
        let action = unsafe { cstr(action) }?.to_string();
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let allowed = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            let schema = reg.get(&model)?;
            Ok(match action.as_str() {
                "read" => rust_store_core::rbac::ensure_read(reg, schema, ctx.as_ref()).is_ok(),
                a => {
                    let wa = rust_store_core::rbac::write_action_from_str(a)?;
                    rust_store_core::rbac::ensure_write(reg, schema, ctx.as_ref(), wa).is_ok()
                }
            })
        })?;
        Ok(json!(allowed))
    })
}

/// RBAC 叠加后的可读字段集（静态 ∩ readFields；data: 排序数组或 null）
#[no_mangle]
pub unsafe extern "C" fn rcore_rbac_readable_fields(
    handle: u64,
    model: *const c_char,
    ctx: *const c_char,
) -> *mut c_char {
    guard(|| {
        let model = unsafe { cstr(model) }?.to_string();
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let fields = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            let schema = reg.get(&model)?;
            let base = rust_store_core::permission::get_readable_fields(schema, ctx.as_ref());
            Ok(rust_store_core::rbac::overlay_readable_fields(
                reg.rbac(),
                &model,
                ctx.as_ref(),
                base,
            ))
        })?;
        Ok(match fields {
            None => json!(null),
            Some(mut s) => {
                let mut v: Vec<String> = s.drain().collect();
                v.sort();
                json!(v)
            }
        })
    })
}

/// RBAC 叠加后的可写字段集（静态 ∩ writeFields；data: 排序数组或 null）
#[no_mangle]
pub unsafe extern "C" fn rcore_rbac_writable_fields(
    handle: u64,
    model: *const c_char,
    ctx: *const c_char,
) -> *mut c_char {
    guard(|| {
        let model = unsafe { cstr(model) }?.to_string();
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let fields = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            let schema = reg.get(&model)?;
            let base = rust_store_core::permission::get_writable_fields(schema, ctx.as_ref());
            Ok(rust_store_core::rbac::overlay_writable_fields(
                reg.rbac(),
                &model,
                ctx.as_ref(),
                base,
            ))
        })?;
        Ok(match fields {
            None => json!(null),
            Some(mut s) => {
                let mut v: Vec<String> = s.drain().collect();
                v.sort();
                json!(v)
            }
        })
    })
}

/// RBAC 行级条件（ownerOnly / condition 的 OR 合并体；data: 对象或 null）。
/// `action ∈ {read, update, remove}`（insert 无行级语义）。
#[no_mangle]
pub unsafe extern "C" fn rcore_rbac_row_condition(
    handle: u64,
    model: *const c_char,
    action: *const c_char,
    ctx: *const c_char,
) -> *mut c_char {
    guard(|| {
        let model = unsafe { cstr(model) }?.to_string();
        let action = unsafe { cstr(action) }?.to_string();
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        if !matches!(action.as_str(), "read" | "update" | "remove") {
            return Err(format!(
                "RBAC row_condition 的 action \"{action}\" 非法（仅支持 read / update / remove）"
            ));
        }
        let cond = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            let schema = reg.get(&model)?;
            Ok(rust_store_core::rbac::row_condition(reg, schema, ctx.as_ref(), &action))
        })?;
        Ok(cond.unwrap_or(Value::Null))
    })
}

#[no_mangle]
pub unsafe extern "C" fn rcore_plan_query(
    handle: u64,
    gql: *const c_char,
    params: *const c_char,
    ctx: *const c_char,
) -> *mut c_char {
    guard(|| {
        let gql = unsafe { cstr(gql) }?;
        let params = unsafe { json_arg(params) }?;
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let plan = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            plan_query(gql, &parse_params(&params)?, reg, ctx.as_ref())
        })?;
        Ok(plan.to_value())
    })
}

#[no_mangle]
pub unsafe extern "C" fn rcore_plan_query_one(
    handle: u64,
    gql: *const c_char,
    params: *const c_char,
    ctx: *const c_char,
) -> *mut c_char {
    guard(|| {
        let gql = unsafe { cstr(gql) }?;
        let params = unsafe { json_arg(params) }?;
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let plan = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            plan_query_one(gql, &parse_params(&params)?, reg, ctx.as_ref())
        })?;
        Ok(plan.to_value())
    })
}

#[no_mangle]
pub unsafe extern "C" fn rcore_plan_query_with_count(
    handle: u64,
    gql: *const c_char,
    params: *const c_char,
    ctx: *const c_char,
) -> *mut c_char {
    guard(|| {
        let gql = unsafe { cstr(gql) }?;
        let params = unsafe { json_arg(params) }?;
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let plan = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            rust_store_core::command::plan_query_with_count(gql, &parse_params(&params)?, reg, ctx.as_ref())
        })?;
        Ok(plan.to_value())
    })
}

#[no_mangle]
pub unsafe extern "C" fn rcore_plan_insert(
    handle: u64,
    schema_name: *const c_char,
    ctx: *const c_char,
    data: *const c_char,
    now: i64,
    new_id: *const c_char,
) -> *mut c_char {
    guard(|| {
        let schema_name = unsafe { cstr(schema_name) }?;
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let data = unsafe { json_arg(data) }?;
        let new_id = unsafe { cstr(new_id) }?.to_string();
        let fn_registry = bridge(handle);
        let plan = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            plan_insert(
                schema_name,
                reg,
                ctx.as_ref(),
                &data,
                now,
                &new_id,
                as_trait(fn_registry.as_ref()),
            )
        })?;
        Ok(plan)
    })
}

#[no_mangle]
pub unsafe extern "C" fn rcore_plan_update(
    handle: u64,
    schema_name: *const c_char,
    ctx: *const c_char,
    condition: *const c_char,
    data: *const c_char,
    now: i64,
    probe: *const c_char,
) -> *mut c_char {
    guard(|| {
        let schema_name = unsafe { cstr(schema_name) }?;
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let condition = unsafe { json_arg(condition) }?;
        let data = unsafe { json_arg(data) }?;
        let probe = unsafe { json_arg(probe) }?;
        let probe = value_to_probe(&probe)?;
        let plan = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            plan_update(schema_name, reg, ctx.as_ref(), &condition, &data, &json!({}), now, probe)
        })?;
        Ok(plan)
    })
}

#[no_mangle]
pub unsafe extern "C" fn rcore_plan_remove(
    handle: u64,
    schema_name: *const c_char,
    ctx: *const c_char,
    condition: *const c_char,
    probe: *const c_char,
) -> *mut c_char {
    guard(|| {
        let schema_name = unsafe { cstr(schema_name) }?;
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let condition = unsafe { json_arg(condition) }?;
        let probe = unsafe { json_arg(probe) }?;
        let probe = value_to_probe(&probe)?;
        let plan = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            plan_remove(schema_name, reg, ctx.as_ref(), &condition, probe)
        })?;
        Ok(plan)
    })
}

#[no_mangle]
pub unsafe extern "C" fn rcore_plan_archive_docs(
    handle: u64,
    schema_name: *const c_char,
    docs: *const c_char,
    now: i64,
) -> *mut c_char {
    guard(|| {
        let schema_name = unsafe { cstr(schema_name) }?;
        let docs = unsafe { json_arg(docs) }?;
        let docs_slice = docs
            .as_array()
            .ok_or_else(|| "docs 必须是文档数组".to_string())?;
        let plan = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            plan_archive_docs(schema_name, reg, docs_slice, now)
        })?;
        Ok(plan)
    })
}

/// backend 取 `"mysql" | "postgres" | "sqlite"`（core `Backend::parse` 语义，未知显式报错）
#[no_mangle]
pub unsafe extern "C" fn rcore_translate(
    handle: u64,
    backend: *const c_char,
    cmd: *const c_char,
) -> *mut c_char {
    guard(|| {
        let backend_s = unsafe { cstr(backend) }?;
        let backend = Backend::parse(backend_s)?;
        let cmd = unsafe { json_arg(cmd) }?;
        let out = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            translate(backend, &cmd, reg)
        })?;
        Ok(out)
    })
}

/// finalize 两段式第一段：同步 fn 内联执行，返回 `{"items":[...],"asyncFnRefs":[...]}`
/// （items 已就地补默认值/同步 fn/权限裁剪；asyncFnRefs 交给宿主批量执行）
#[no_mangle]
pub unsafe extern "C" fn rcore_finalize_prepare(
    handle: u64,
    postprocess: *const c_char,
    items: *const c_char,
    ctx: *const c_char,
) -> *mut c_char {
    guard(|| {
        let post = unsafe { json_arg(postprocess) }?;
        let items_v = unsafe { json_arg(items) }?;
        let mut items: Vec<Value> = serde_json::from_value(items_v)
            .map_err(|e| format!("items 必须是文档数组: {e}"))?;
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let fn_registry = bridge(handle);
        let refs = with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            prepare_query_wrap(&post, &mut items, reg, as_trait(fn_registry.as_ref()), ctx.as_ref())
        })?;
        Ok(json!({ "items": items, "asyncFnRefs": refs }))
    })
}

fn prepare_query_wrap(
    post: &Value,
    items: &mut Vec<Value>,
    reg: &Registry,
    fn_registry: Option<&dyn FnRegistry>,
    ctx: Option<&Context>,
) -> Result<Vec<String>, String> {
    rust_store_core::command::prepare_query(post, items, reg, fn_registry, ctx)
}

/// finalize 第三段：剥离依赖注入字段（纯函数）
#[no_mangle]
pub unsafe extern "C" fn rcore_finalize_strip(
    postprocess: *const c_char,
    items: *const c_char,
) -> *mut c_char {
    guard(|| {
        let post = unsafe { json_arg(postprocess) }?;
        let items_v = unsafe { json_arg(items) }?;
        let mut items: Vec<Value> = serde_json::from_value(items_v)
            .map_err(|e| format!("items 必须是文档数组: {e}"))?;
        rust_store_core::command::strip_query(&post, &mut items);
        Ok(json!(items))
    })
}

/// 两阶段排序还原（纯函数）
#[no_mangle]
pub unsafe extern "C" fn rcore_restore_sort_order(
    items: *const c_char,
    ids: *const c_char,
    sort: *const c_char,
) -> *mut c_char {
    guard(|| {
        let items_v = unsafe { json_arg(items) }?;
        let mut items: Vec<Value> = serde_json::from_value(items_v)
            .map_err(|e| format!("items 必须是文档数组: {e}"))?;
        let ids = unsafe { json_arg(ids) }?;
        let ids: Vec<Value> = serde_json::from_value(ids)
            .map_err(|e| format!("ids 必须是数组: {e}"))?;
        let sort = unsafe { json_arg(sort) }?;
        let sort_opt = if sort.is_null() { None } else { Some(&sort) };
        restore_sort_order(&mut items, &ids, sort_opt);
        Ok(json!(items))
    })
}

/// SQL 行数组按 rowShape 还原为嵌套文档（纯函数）
#[no_mangle]
pub unsafe extern "C" fn rcore_restore_rows_json(
    row_shape: *const c_char,
    rows: *const c_char,
) -> *mut c_char {
    guard(|| {
        let shape = unsafe { json_arg(row_shape) }?;
        let rows = unsafe { json_arg(rows) }?;
        let restored = restore_rows_json(&shape, &rows)?;
        Ok(restored)
    })
}

/// 便捷入口：一次性 finalize（core 内直跑 asyncFn；仅同步单机宿主建议使用，
/// go-store 走两段式以保持与 py/node 宿主一致的回调语义）
#[no_mangle]
pub unsafe extern "C" fn rcore_finalize_query(
    handle: u64,
    postprocess: *const c_char,
    items: *const c_char,
    ctx: *const c_char,
) -> *mut c_char {
    guard(|| {
        let post = unsafe { json_arg(postprocess) }?;
        let items_v = unsafe { json_arg(items) }?;
        let mut items: Vec<Value> = serde_json::from_value(items_v)
            .map_err(|e| format!("items 必须是文档数组: {e}"))?;
        let ctx = unsafe { json_arg(ctx) }?;
        let ctx = value_to_ctx(&ctx)?;
        let fn_registry = bridge(handle);
        with_registries(|map| {
            let reg = map
                .get(&(handle as usize))
                .ok_or_else(|| format!("registry 句柄 {handle} 不存在"))?;
            finalize_query(&post, &mut items, reg, as_trait(fn_registry.as_ref()), ctx.as_ref())
        })?;
        Ok(json!(items))
    })
}
