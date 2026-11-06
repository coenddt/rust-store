//! D1 / D2 / D3 / D4：默认值、显式 null 与对象/数组规则回归。
//!
//! - D1：`insert.returns` 必须等于落库 doc（+ 计算列），不再补类型零值/凭空造嵌套对象；
//! - D2：读路径只在「键缺失」时回填 default，显式 null 可原样读回；
//! - D3：object 字段收到非对象值 → 显式报错；数组不递归（schema 无元素定义，规则明确）；
//! - D4：`processNode` 的无法解析花括号节点与 `planQuery` 一致显式报错，禁止静默丢字段。

use serde_json::{json, Value};

use rust_store_core::command::plan_insert;
use rust_store_core::computes::process_node;
use rust_store_core::pipeline::{parse, tokenize};
use rust_store_core::schema::Registry;

fn registry_with(defn: Value) -> Registry {
    let mut reg = Registry::new();
    reg.register(&defn).expect("schema 注册应成功");
    reg
}

fn defaults_schema() -> Value {
    json!({
        "name": "T",
        "collection": "ts",
        "idPrefix": "t",
        "timestamps": false,
        "fields": {
            "s": { "type": "string" },
            "n": { "type": "int", "default": 5 },
            "b": { "type": "boolean" },
            "arr": { "type": "array" },
            "nest": { "type": "object", "fields": { "x": { "type": "int", "default": 7 }, "y": { "type": "string" } } },
            "d": { "type": "string", "default": "D" }
        },
        "relations": {}
    })
}

fn insert(reg: &Registry, data: Value) -> Value {
    plan_insert("T", reg, None, &data, 0, "t1", None).expect("insert 应规划成功")
}

fn process(reg: &Registry, gql: &str, doc: Value) -> Result<Value, String> {
    let mut ast = parse(&tokenize(gql))?;
    let schema = reg.get(&ast.model)?;
    let mut doc = doc;
    process_node(
        &mut doc,
        &mut ast.fields,
        &mut ast.relations,
        schema,
        None,
        reg,
        None,
    )?;
    Ok(doc)
}

// ─── D1：returns == 落库 doc ────────────────────────────────

#[test]
fn d1_insert_returns_equals_stored_doc() {
    let reg = registry_with(defaults_schema());
    let plan = insert(&reg, json!({}));
    let doc = &plan["command"]["doc"];
    assert_eq!(
        &plan["returns"], doc,
        "returns 必须等于落库 doc（默认值已物化，不得补类型零值）"
    );
    assert_eq!(doc, &json!({ "_id": "t1", "d": "D", "n": 5 }));
}

#[test]
fn d1_insert_returns_keeps_explicit_null() {
    let reg = registry_with(defaults_schema());
    let plan = insert(
        &reg,
        json!({ "s": null, "d": null, "n": null, "nest": { "x": null, "y": null } }),
    );
    let doc = &plan["command"]["doc"];
    assert_eq!(
        &plan["returns"], doc,
        "显式 null 不得在 returns 中变回默认值"
    );
    assert_eq!(doc["d"], Value::Null);
    assert_eq!(doc["n"], Value::Null);
    assert_eq!(doc["nest"]["x"], Value::Null);
}

// ─── D2：读路径只在缺键时补默认 ─────────────────────────────

#[test]
fn d2_read_does_not_replace_explicit_null() {
    let reg = registry_with(defaults_schema());
    let out = process(
        &reg,
        "T{ d n nest{ x } }",
        json!({ "_id": "t2", "d": null, "n": null, "nest": { "x": null } }),
    )
    .expect("processNode 应成功");
    assert_eq!(out["d"], Value::Null, "显式 null 必须可读回: {out}");
    assert_eq!(out["n"], Value::Null, "显式 null 必须可读回: {out}");
    assert_eq!(
        out["nest"]["x"],
        Value::Null,
        "点号子字段显式 null 必须可读回: {out}"
    );
}

#[test]
fn d2_read_fills_default_when_key_missing() {
    let reg = registry_with(defaults_schema());
    let out = process(&reg, "T{ d n }", json!({ "_id": "t3" })).expect("processNode 应成功");
    assert_eq!(out["d"], "D", "缺键应补声明默认: {out}");
    assert_eq!(out["n"], 5, "缺键应补声明默认: {out}");
}

// ─── D3：object 类型校验 / 数组不递归 ───────────────────────

#[test]
fn d3_object_field_rejects_non_object_value() {
    let reg = registry_with(defaults_schema());
    let err = plan_insert("T", &reg, None, &json!({ "nest": "str" }), 0, "t4", None)
        .expect_err("object 字段收到非对象值应显式报错");
    assert!(err.contains("声明为 object"), "{err}");
}

#[test]
fn d3_array_items_are_not_rewritten() {
    let reg = registry_with(defaults_schema());
    let items = json!([{ "z": 1 }, { "z": null }]);
    let plan = insert(&reg, json!({ "arr": items.clone() }));
    assert_eq!(
        plan["command"]["doc"]["arr"], items,
        "数组元素内部不应用默认值（规则在 object 上，二者一致）"
    );
}

// ─── D4：无法解析的花括号节点显式报错 ──────────────────────

#[test]
fn d4_process_node_rejects_braces_on_array_field() {
    let mut defn = defaults_schema();
    // arr 仍是数组标量字段，没有 relations.arr
    defn["fields"]["arr"] = json!({ "type": "array" });
    let reg = registry_with(defn);
    let err = process(
        &reg,
        "T{ arr{ z } }",
        json!({ "_id": "t5", "arr": [{ "z": 1 }] }),
    )
    .expect_err("arr{ z } 不得静默丢弃");
    assert!(
        err.contains("关系 \"arr\" 未在 schema \"T\" 中定义"),
        "错误信息应与 planQuery 一致: {err}"
    );
}
