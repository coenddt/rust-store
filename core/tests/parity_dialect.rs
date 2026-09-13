//! dialect 对拍测试（Mongo 命令 → 关系型 SQL）
//!
//! 核心 parity 性质：同一命令在 MySQL / PostgreSQL / SQLite 三端，参数化 SQL 在归一化后
//! 语义一致（只差标识符引号与占位符风格）；`restore_rows`/`introspect`/`overlay` 为纯逻辑，
//! 四侧可复现。此测试把输入以 JSON 内联，作断言基准（无外部 golden；原 JS 参考实现已退役，
//! 其黄金基准为冻结快照，复算校验见 `node tools/verify-fixtures.js`）。

use serde_json::{json, Map, Value};

use rust_store_core::dialect::{
    introspect_to_schema_json, merge_schema, restore_rows_json, translate, Backend,
};
use rust_store_core::pipeline::{build_pipeline, parse_gql};
use rust_store_core::schema::Registry;

fn registry_with(schemas: &[Value]) -> Registry {
    let mut r = Registry::new();
    for s in schemas {
        r.register(s).expect("schema 注册失败");
    }
    r
}

/// 归一化 SQL：去标识符引号、占位符归一为 `?`、折叠空白、小写（仅用于跨端语义对比）
fn normalize(sql: &str) -> String {
    let s = sql.replace(['`', '"'], "");
    // `$n` → `?`
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
            while i < chars.len() && chars[i].is_ascii_digit()
                || (i < chars.len() && chars[i] == '$')
            {
                i += 1;
            }
            out.push('?');
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

const SCHEMAS: &str = r#"[
  {
    "name": "Post", "collection": "posts", "timestamps": false,
    "fields": { "title": { "type": "string" }, "status": { "type": "string" }, "views": { "type": "int" } },
    "relations": {}
  },
  {
    "name": "Order", "collection": "orders", "timestamps": false,
    "fields": { "code": { "type": "string" }, "amount": { "type": "float" } },
    "relations": {
      "items": { "model": "OrderItem", "type": "many", "localField": "_id", "foreignField": "orderId" }
    }
  },
  {
    "name": "OrderItem", "collection": "order_items", "timestamps": false,
    "fields": { "orderId": { "type": "string" }, "sku": { "type": "string" }, "qty": { "type": "int" } },
    "relations": {}
  }
]"#;

fn schemas() -> Vec<Value> {
    serde_json::from_str(SCHEMAS).expect("内联 schemas 解析失败")
}

fn assert_sql_parity(name: &str, cmd: &Value) {
    let registry = registry_with(&schemas());
    let mut base: Option<String> = None;
    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, cmd, &registry).expect(name);
        let stmts = out
            .get("stmts")
            .and_then(|s| s.as_array())
            .expect("stmts 数组");
        assert_eq!(stmts.len(), 1, "[{}] 应恰一条语句", name);
        let text = stmts[0]
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let norm = normalize(&text);
        match &base {
            None => base = Some(norm),
            Some(b) => assert_eq!(&norm, b, "[{}] 跨后端 SQL 不一致", name),
        }
    }
}

#[test]
fn dialect_cross_backend_sql_parity() {
    assert_sql_parity(
        "find-simple",
        &json!({ "kind": "find", "collection": "posts",
                 "filter": { "status": "draft" },
                 "projection": { "title": 1, "status": 1 } }),
    );
    assert_sql_parity(
        "find-comparison",
        &json!({ "kind": "find", "collection": "posts",
                 "filter": { "views": { "$gte": 10 }, "status": "draft" },
                 "projection": null }),
    );
    assert_sql_parity(
        "count",
        &json!({ "kind": "countDocuments", "collection": "posts",
                 "filter": { "status": "draft" } }),
    );
    assert_sql_parity(
        "insert-one",
        &json!({ "kind": "insertOne", "collection": "posts",
                 "doc": { "title": "hello", "status": "draft", "views": 5 } }),
    );
    assert_sql_parity(
        "insert-many",
        &json!({ "kind": "insertMany", "collection": "posts",
                 "docs": [
                    { "title": "a", "status": "draft", "views": 1 },
                    { "title": "b", "status": "draft", "views": 2 }
                 ] }),
    );
}

#[test]
fn dialect_aggregate_lookup_sql_parity() {
    assert_sql_parity(
        "aggregate-lookup",
        &json!({ "kind": "aggregate", "collection": "orders", "pipeline": [
            { "$match": { "amount": { "$gt": 0 } } },
            { "$lookup": { "from": "order_items", "localField": "_id",
                           "foreignField": "orderId", "as": "items" } },
            { "$sort": { "code": 1 } }
        ] }),
    );
}

/// 可下推的 aggregate（无每父 top-N）→ `unsupported` 必须为空
#[test]
fn dialect_translate_supported_aggregate_has_no_unsupported() {
    let registry = registry_with(&schemas());
    let out = translate(
        Backend::Sqlite,
        &json!({ "kind": "aggregate", "collection": "orders", "pipeline": [
            { "$lookup": { "from": "order_items", "localField": "_id",
                           "foreignField": "orderId", "as": "items" } }
        ] }),
        &registry,
    )
    .expect("translate");
    assert_eq!(
        out.get("unsupported")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(0),
        "无子 limit 的 $lookup 不应产生 unsupported: {}",
        out
    );
}

/// `$lookup` 子 `$limit`（每父 top-N）→ **窗口函数下推**
/// （`ROW_NUMBER() OVER (PARTITION BY <fk> ORDER BY …)`），不再标记 `childLimit` 不支持；
/// 跨后端 SQL 归一后语义一致。
#[test]
fn dialect_translate_child_limit_pushed_down_as_window() {
    let registry = registry_with(&schemas());
    let mut base: Option<String> = None;
    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(
            backend,
            &json!({ "kind": "aggregate", "collection": "orders", "pipeline": [
                { "$lookup": { "from": "order_items", "as": "items",
                               "let": { "rel__id": "$_id" },
                               "pipeline": [
                                   { "$match": { "$expr": { "$eq": ["$orderId", "$$rel__id"] } } },
                                   { "$sort": { "qty": -1 } },
                                   { "$limit": 3 }
                               ] } }
            ] }),
            &registry,
        )
        .expect("translate");

        assert_eq!(
            out.get("unsupported")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(0),
            "[{:?}] 子 limit 应已下推（无 unsupported）: {}",
            backend,
            out
        );
        let text = out["stmts"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("ROW_NUMBER() OVER (PARTITION BY"),
            "[{:?}] 应下推为窗口函数: {}",
            backend,
            text
        );
        assert!(
            text.contains("JOIN"),
            "[{:?}] 应下推子表 JOIN: {}",
            backend,
            text
        );
        let norm = normalize(text);
        match &base {
            None => base = Some(norm),
            Some(b) => assert_eq!(&norm, b, "[{:?}] 跨后端窗口下推 SQL 不一致", backend),
        }
    }
}

#[test]
fn dialect_restore_rows_from_flat_lines() {
    let registry = registry_with(&schemas());
    // aggregate + $lookup（orders → items）
    let out = translate(
        Backend::Sqlite,
        &json!({ "kind": "aggregate", "collection": "orders", "pipeline": [
            { "$lookup": { "from": "order_items", "localField": "_id",
                           "foreignField": "orderId", "as": "items" } }
        ] }),
        &registry,
    )
    .expect("translate");
    let stmts = out.get("stmts").and_then(|s| s.as_array()).expect("stmts");
    let shape = stmts[0].get("rowShape").cloned().expect("rowShape");

    // 两条订单、第一条带 2 个 item、第二条含 null（LEFT JOIN 无匹配）
    // 关系列 alias = <as>_<i>_<field>，i 为 JOIN 序号（单个 lookup → 0）
    let rows = json!([
        { "_id": "A", "code": "A-1", "items_0_sku": "sku-a", "items_0_qty": 2 },
        { "_id": "A", "code": "A-1", "items_0_sku": "sku-b", "items_0_qty": 3 },
        { "_id": "B", "code": "B-1", "items_0_sku": null,    "items_0_qty": null },
    ]);

    let restored = restore_rows_json(&shape, &rows).expect("restore");

    let arr = restored.as_array().expect("还原应为数组");
    assert_eq!(arr.len(), 2, "应有两条订单: {}", restored);
    // 订单 A-1 合并出 items 数组
    let has_a1 = arr.iter().any(|d| {
        d.get("items")
            .and_then(|v| v.as_array())
            .map(|a| a.len() == 2)
            .unwrap_or(false)
    });
    assert!(has_a1, "A-1 应还原出 2 个 items: {}", restored);
    // 空 items：item 字段全 null → 不生成空对象
    let has_b1 = arr
        .iter()
        .any(|d| d.get("code").and_then(|v| v.as_str()) == Some("B-1"));
    assert!(has_b1, "B-1 应被还原: {}", restored);
}

#[test]
fn dialect_restore_rows_roundtrip_supports_is_array() {
    // 直接构造数组形状（不依赖 translate），验证 is_array 聚合路径
    let shape = json!({
        "columns": [
            { "alias": "_id",    "path": ["_id"],         "isArray": false, "subShape": null },
            { "alias": "code",   "path": ["code"],        "isArray": false, "subShape": null },
            { "alias": "it_0_sku","path": ["items","sku"],"isArray": true,  "subShape": null }
        ]
    });
    let rows = json!([
        { "_id": "1", "code": "A", "it_0_sku": "x" },
        { "_id": "1", "code": "A", "it_0_sku": "y" },
    ]);
    let restored = restore_rows_json(&shape, &rows).expect("restore");
    let arr = restored.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(
        arr[0]
            .get("items")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        2
    );
}

#[test]
fn dialect_introspection_to_schema_json() {
    let rows = json!({
        "tables": [ { "name": "posts" }, { "name": "order_items" } ],
        "columns": [
            { "table": "posts",       "name": "_id",       "type": "TEXT", "notnull": 1, "pk": 1 },
            { "table": "posts",       "name": "title",     "type": "TEXT", "notnull": 0, "pk": 0 },
            { "table": "posts",       "name": "views",     "type": "INTEGER", "notnull": 0, "pk": 0 },
            { "table": "order_items", "name": "_id",       "type": "TEXT", "notnull": 1, "pk": 1 },
            { "table": "order_items", "name": "order_id",  "type": "TEXT", "notnull": 1, "pk": 0 },
            { "table": "order_items", "name": "sku",       "type": "TEXT", "notnull": 0, "pk": 0 }
        ],
        "fks": [
            { "table": "order_items", "column": "order_id", "refTable": "posts", "refColumn": "_id" }
        ],
        "indexes": []
    });
    let schema = introspect_to_schema_json(&rows, &Backend::Sqlite).expect("introspect");

    let arr = schema.as_array().expect("schema 数组");
    // posts 应含 _id/title/views，views 映射为 number
    let posts = arr
        .iter()
        .find(|d| d.get("name").and_then(|v| v.as_str()) == Some("posts"))
        .expect("posts def");
    let views = posts
        .get("fields")
        .and_then(|f| f.get("views").and_then(|v| v.get("type")))
        .and_then(|v| v.as_str());
    assert_eq!(views, Some("number"), "views 应为 number");
    // order_items 主键缺省 _id，外键 order_id → 对 posts 的 one 关系已生成
    let oi = arr
        .iter()
        .find(|d| d.get("name").and_then(|v| v.as_str()) == Some("order_items"))
        .expect("order_items def");
    let rel = oi
        .get("relations")
        .and_then(|r| r.get("posts"))
        .expect("posts 关系");
    assert_eq!(rel.get("type").and_then(|v| v.as_str()), Some("one"));
    // posts 侧应由第二轮补反向 many
    let rels = posts
        .get("relations")
        .and_then(|r| r.as_object())
        .expect("posts relations");
    assert!(
        rels.contains_key("order_items"),
        "posts 应反向含 order_items 关系: {:#?}",
        rels
    );
}

#[test]
fn dialect_overlay_merge_compute_and_permission() {
    let base = json!([
        { "name": "posts", "collection": "posts", "fields": { "title": { "type": "string" } }, "relations": {} }
    ]);
    let overlay = json!([
        { "name": "posts",
          "fields": { "views": { "type": "int", "default": 0 } },
          "computes": { "slug": { "fn": true, "depends": ["title"] } },
          "read": { "roles": ["admin", "editor"] } }
    ]);
    let merged = merge_schema(&base, &overlay).expect("merge");
    let def = merged.as_array().unwrap().first().unwrap();
    // 计算列注入
    let computes = def.get("computes").expect("computes 存在");
    assert!(computes.get("slug").is_some());
    // 字段并集
    let fields = def
        .get("fields")
        .and_then(|f| f.as_object())
        .expect("fields");
    assert!(fields.contains_key("title") && fields.contains_key("views"));
    // read 覆盖
    assert_eq!(
        def.get("read")
            .and_then(|r| r.get("roles").and_then(|v| v.as_array()).map(|a| a.len())),
        Some(2)
    );
}

// ─── M-8-1：仅 $skip 无 $limit → 按后端生成「取到末尾」的合法惯用法 ──────────

#[test]
fn dialect_aggregate_skip_without_limit_uses_backend_idiom() {
    let registry = registry_with(&schemas());
    let cmd =
        json!({ "kind": "aggregate", "collection": "posts", "pipeline": [ { "$skip": 10 } ] });

    // PostgreSQL：LIMIT 不接受负数 → 合法形态是省略 LIMIT、只写裸 OFFSET
    let pg = translate(Backend::Postgres, &cmd, &registry).expect("pg translate");
    let pg_text = pg["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        pg_text.contains(" OFFSET $1"),
        "PG 应为裸 OFFSET: {pg_text}"
    );
    assert!(
        !pg_text.contains("LIMIT"),
        "PG 不得出现 LIMIT（更不得出现负 LIMIT）: {pg_text}"
    );

    // MySQL：LIMIT 不接受负数 → 官方「取到末尾」惯用法：大数 LIMIT（2^64-1）+ OFFSET
    let my = translate(Backend::Mysql, &cmd, &registry).expect("mysql translate");
    let my_text = my["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        my_text.contains("LIMIT 18446744073709551615 OFFSET ?"),
        "MySQL 应为大数 LIMIT 惯用法: {my_text}"
    );

    // SQLite：`LIMIT -1` 官方语义即「不限制行数」，合法保留
    let lite = translate(Backend::Sqlite, &cmd, &registry).expect("sqlite translate");
    let lite_text = lite["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        lite_text.contains("LIMIT -1 OFFSET ?"),
        "SQLite LIMIT -1 合法: {lite_text}"
    );
}

// ─── M-8-3：$regex + $options 的后端语义合并处理 ─────────────

#[test]
fn translate_regex_with_options_i_backend_semantics() {
    let registry = registry_with(&schemas());
    let cmd = json!({ "kind": "find", "collection": "posts",
                      "filter": { "title": { "$regex": "abc", "$options": "i" } } });

    // PostgreSQL：'i' → 大小写不敏感运算符 ~*，语义正确表达、无告警
    let pg = translate(Backend::Postgres, &cmd, &registry).expect("pg translate");
    let pg_text = pg["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(pg_text.contains("~*"), "PG 应翻译为 ~*: {pg_text}");
    assert!(
        pg.get("warnings")
            .and_then(|w| w.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "PG 'i' 可表达，不应有告警: {pg}"
    );

    // MySQL：'i' → REGEXP_LIKE(col, ?, 'i')（MySQL 8 REGEXP 默认区分大小写，不能靠 collation）
    let my = translate(Backend::Mysql, &cmd, &registry).expect("mysql translate");
    let my_text = my["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        my_text.contains("REGEXP_LIKE") && my_text.contains("'i'"),
        "MySQL 应翻译为 REGEXP_LIKE(col, ?, 'i'): {my_text}"
    );

    // SQLite：不支持 flags → 条件保留（REGEXP）+ warnings 明示降级，绝不静默
    let lite = translate(Backend::Sqlite, &cmd, &registry).expect("sqlite translate");
    let lite_text = lite["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        lite_text.contains("REGEXP"),
        "SQLite 应保留 REGEXP 条件: {lite_text}"
    );
    let warns = lite
        .get("warnings")
        .and_then(|w| w.as_array())
        .expect("warnings 数组");
    assert_eq!(warns.len(), 1, "SQLite + 'i' 应恰一条告警: {lite}");
    assert!(
        warns[0].as_str().unwrap_or("").contains("i"),
        "告警应指明无法表达的 flags: {warns:?}"
    );
}

// ─── M-8-3：写路径（无告警通道）对不可表达 $options 的 fail-fast ──────────

#[test]
fn translate_write_rejects_unexpressible_regex_options() {
    let registry = registry_with(&schemas());
    let cmd = json!({ "kind": "deleteMany", "collection": "posts",
                      "filter": { "title": { "$regex": "x", "$options": "i" } } });

    // SQLite 无法表达 'i' 且写路径无告警通道 → 显式报错，绝不静默写错行
    let err = translate(Backend::Sqlite, &cmd, &registry)
        .expect_err("SQLite + $options 'i' 在写路径必须显式报错");
    assert!(err.contains("$options"), "错误应指明 $options: {err}");

    // PostgreSQL 可表达 'i' → 写路径正常翻译（不受影响）
    let pg = translate(Backend::Postgres, &cmd, &registry).expect("pg translate");
    let pg_text = pg["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(pg_text.contains("~*"), "PG 写路径应正常翻译 ~*: {pg_text}");
}

// ─── m-8-1：introspect 不再产出 __pk_col 幻影字段 ─────────────

#[test]
fn dialect_introspection_non_id_pk_has_no_phantom_field() {
    // 非 _id 命名主键（如 uid）：此前会补写 "__pk_col": "uid" 裸字符串键，
    // 被 normalize_fields 当「类型简写」解析成幻影字段（全库无消费方），已删除
    let rows = json!({
        "tables": [ { "name": "users" } ],
        "columns": [
            { "table": "users", "name": "uid",  "type": "TEXT", "notnull": 1, "pk": 1 },
            { "table": "users", "name": "name", "type": "TEXT", "notnull": 0, "pk": 0 }
        ],
        "fks": [],
        "indexes": []
    });
    let schema = introspect_to_schema_json(&rows, &Backend::Sqlite).expect("introspect");
    let def = schema
        .as_array()
        .and_then(|a| a.first())
        .expect("users def");
    let fields = def
        .get("fields")
        .and_then(|f| f.as_object())
        .expect("fields 对象");
    assert!(
        !fields.contains_key("__pk_col"),
        "不得产出 __pk_col 幻影字段: {fields:?}"
    );
    assert!(
        fields.contains_key("_id"),
        "主键仍应映射为 _id 字段: {fields:?}"
    );
    assert!(fields.contains_key("name"), "普通字段不受影响: {fields:?}");
}

// ─── 第 10 轮缺陷修复回归（B-10-1 / C-10-1 / C-10-2 / M-10-1） ─────────

/// B-10-1：非对象 filter 必须显式报错，绝不静默退化为「无过滤」——
/// 后者会让写路径产出无 WHERE 的无界 DELETE/UPDATE（一票否决：数据破坏风险）。
#[test]
fn dialect_rejects_non_object_filter() {
    let registry = registry_with(&schemas());

    // 写路径：字符串 filter → 修复前会产出 `DELETE FROM "posts" AS t`（全表删）
    let err = translate(
        Backend::Postgres,
        &json!({ "kind": "deleteMany", "collection": "posts", "filter": "oops" }),
        &registry,
    )
    .expect_err("非对象 filter 必须报错（不得静默全表删）");
    assert!(err.contains("filter"), "错误应指明 filter: {err}");

    // 读路径：数字 / 数组 / 布尔 filter 同样拒绝
    for cmd in [
        json!({ "kind": "find", "collection": "posts", "filter": 5 }),
        json!({ "kind": "countDocuments", "collection": "posts", "filter": [1, 2] }),
        json!({ "kind": "updateMany", "collection": "posts", "filter": true,
                "update": { "$set": { "status": "x" } } }),
    ] {
        translate(Backend::Postgres, &cmd, &registry).expect_err("非对象 filter 必须报错");
    }

    // `null` 仍表示「无过滤条件」（与 `{}` 同义）——既有约定不回归
    let out = translate(
        Backend::Postgres,
        &json!({ "kind": "find", "collection": "posts", "filter": null }),
        &registry,
    )
    .expect("null filter 应放行");
    let text = out["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        !text.contains("WHERE"),
        "null filter 不应产生 WHERE: {text}"
    );
}

/// C-10-1：同一对象内的逻辑组（$and/$or/$nor）与兄弟字段条件必须 AND 合并，
/// 绝不丢弃兄弟条件（修复前逻辑组命中即提前 return）。
#[test]
fn dialect_filter_logical_group_keeps_sibling_conditions() {
    let registry = registry_with(&schemas());

    // $and + 兄弟字段：WHERE 必须同时含 views 与 status
    let cmd = json!({ "kind": "find", "collection": "posts",
        "filter": { "status": "draft", "$and": [ { "views": { "$gt": 1 } } ] } });
    let out = translate(Backend::Postgres, &cmd, &registry).expect("translate");
    let text = out["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("t.\"views\" > $1") && text.contains("t.\"status\" = $2"),
        "兄弟条件 status 不得被丢弃: {text}"
    );
    assert_eq!(
        out["stmts"][0]["params"].as_array().map(|a| a.len()),
        Some(2),
        "应有两个绑定参数: {out}"
    );

    // $or 同理（兄弟字段 + OR 组同时生效）
    let cmd = json!({ "kind": "find", "collection": "posts",
        "filter": { "status": "draft",
                    "$or": [ { "views": { "$gt": 1 } }, { "views": { "$lt": 5 } } ] } });
    let out = translate(Backend::Postgres, &cmd, &registry).expect("translate");
    let text = out["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("OR") && text.contains("t.\"status\""),
        "$or 与兄弟字段应同时生效: {text}"
    );
    assert_eq!(
        out["stmts"][0]["params"].as_array().map(|a| a.len()),
        Some(3),
        "应为三个绑定参数: {out}"
    );

    // $nor 同理
    let cmd = json!({ "kind": "find", "collection": "posts",
        "filter": { "status": "draft", "$nor": [ { "views": { "$gt": 1 } } ] } });
    let out = translate(Backend::Postgres, &cmd, &registry).expect("translate");
    let text = out["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("NOT") && text.contains("t.\"status\""),
        "$nor 与兄弟字段应同时生效: {text}"
    );
}

/// C-10-2：insertMany 的列集必须是**所有文档字段的并集**（异构文档不得丢列）。
#[test]
fn dialect_insert_many_merges_heterogeneous_columns() {
    let registry = registry_with(&schemas());
    let cmd = json!({ "kind": "insertMany", "collection": "posts",
        "docs": [ { "title": "a" }, { "title": "b", "views": 9 } ] });

    // 跨后端 SQL 一致
    assert_sql_parity("insert-many-heterogeneous", &cmd);

    let out = translate(Backend::Postgres, &cmd, &registry).expect("translate");
    let text = out["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("\"title\"") && text.contains("\"views\""),
        "列集应为并集（含仅次篇文档才有的 views）: {text}"
    );
    let params = out["stmts"][0]["params"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        params.contains(&json!(9)),
        "第二篇文档的 views=9 不得丢失: {params:?}"
    );
}

/// M-10-1：$sort 的关系点号字段必须映射到 JOIN 别名，绝不产出无效列 `t."items.qty"`。
#[test]
fn dialect_aggregate_sort_by_relation_uses_join_alias() {
    let registry = registry_with(&schemas());

    // 有对应 $lookup → 排序键映射到 r0 别名
    let out = translate(
        Backend::Postgres,
        &json!({ "kind": "aggregate", "collection": "orders", "pipeline": [
            { "$lookup": { "from": "order_items", "localField": "_id",
                           "foreignField": "orderId", "as": "items" } },
            { "$sort": { "items.qty": -1 } }
        ] }),
        &registry,
    )
    .expect("translate");
    let text = out["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("ORDER BY r0.\"qty\" DESC"),
        "关系排序应映射到 JOIN 别名: {text}"
    );
    assert!(
        !text.contains("items.qty"),
        "不得产出无效列 t.\"items.qty\": {text}"
    );
    assert_eq!(
        out["unsupported"].as_array().map(|a| a.len()),
        Some(0),
        "可下推的关系排序不应产生 unsupported: {out}"
    );

    // 无对应 $lookup → 不下推：告警 + sortField 标记，且不得出现无效列
    let out = translate(
        Backend::Postgres,
        &json!({ "kind": "aggregate", "collection": "orders",
                 "pipeline": [ { "$sort": { "items.qty": -1 } } ] }),
        &registry,
    )
    .expect("translate");
    let text = out["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(!text.contains("items.qty"), "不得产出无效列: {text}");
    let unsupported = out["unsupported"].as_array().cloned().unwrap_or_default();
    assert_eq!(unsupported.len(), 1, "应标记 1 条 unsupported: {out}");
    assert_eq!(
        unsupported[0].get("code").and_then(|v| v.as_str()),
        Some("sortField"),
        "code 应为 sortField: {out}"
    );
    assert!(
        !out["warnings"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .is_empty(),
        "应同时给出 warning: {out}"
    );

    // 根表标量排序既有行为不回归
    let out = translate(
        Backend::Postgres,
        &json!({ "kind": "aggregate", "collection": "orders",
                 "pipeline": [ { "$sort": { "amount": -1 } } ] }),
        &registry,
    )
    .expect("translate");
    let text = out["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("ORDER BY t.\"amount\" DESC"),
        "标量排序应保持: {text}"
    );
}

// ─── §9.2(1) 根级 `$group` / `$having` 的 SQL 下推 ───────────────

/// `$group`（单键 by + 多 agg）→ `GROUP BY` + 聚合列；跨后端 SQL 归一后一致。
#[test]
fn dialect_aggregate_group_sql_parity() {
    let registry = registry_with(&schemas());
    let cmd = json!({ "kind": "aggregate", "collection": "posts", "pipeline": [
        { "$match": { "status": "draft" } },
        { "$group": { "_id": "$status", "n": { "$sum": 1 }, "total": { "$sum": "$views" } } },
        { "$project": { "_id": 0, "n": 1, "status": "$_id", "total": 1 } }
    ] });

    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, &cmd, &registry).expect("group translate");
        assert_eq!(
            out["unsupported"].as_array().map(|a| a.len()),
            Some(0),
            "[{backend:?}] 可下推的分组聚合不应产生 unsupported: {out}"
        );
        let text = out["stmts"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("COUNT(*)"),
            "[{backend:?}] 应为 COUNT(*): {text}"
        );
        assert!(
            text.contains("GROUP BY t.\"status\"") || text.contains("GROUP BY t.`status`"),
            "[{backend:?}] 应按分组键 GROUP BY: {text}"
        );
        assert!(
            text.contains("SUM("),
            "[{backend:?}] 应为 SUM 聚合列: {text}"
        );
    }
    assert_sql_parity("aggregate-group", &cmd);

    // 行还原：分组结果无 `_id` 列 → 每行独立成篇（不得合并成一篇）
    let out = translate(Backend::Sqlite, &cmd, &registry).expect("group translate");
    let shape = out["stmts"][0]["rowShape"].clone();
    let rows = json!([
        { "b0": "draft", "a0": 3, "a1": 42 },
        { "b0": "published", "a0": 7, "a1": null },
    ]);
    let restored = restore_rows_json(&shape, &rows).expect("restore");
    let arr = restored.as_array().expect("数组");
    assert_eq!(arr.len(), 2, "分组结果每行一篇文档: {restored}");
    assert_eq!(arr[0].get("status"), Some(&json!("draft")));
    assert_eq!(arr[0].get("n"), Some(&json!(3)));
    // 空集语义（§9.7）：`$sum` → 显式 null（而非缺失键）
    assert_eq!(arr[1].get("total"), Some(&Value::Null), "{restored}");
}

/// `$having` → `HAVING`（重复聚合表达式）；分组后 `$sort` → `ORDER BY`（照 by/agg 域解析）；
/// `$limit` → `LIMIT`。
#[test]
fn dialect_aggregate_group_having_sort_limit() {
    let registry = registry_with(&schemas());
    let cmd = json!({ "kind": "aggregate", "collection": "posts", "pipeline": [
        { "$group": { "_id": "$status", "n": { "$sum": 1 }, "total": { "$sum": "$views" } } },
        { "$match": { "n": { "$gt": 1 } } },
        { "$sort": { "total": -1 } },
        { "$limit": 10 },
        { "$project": { "_id": 0, "status": "$_id", "n": 1, "total": 1 } }
    ] });

    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, &cmd, &registry).expect("group having translate");
        assert_eq!(
            out["unsupported"].as_array().map(|a| a.len()),
            Some(0),
            "[{backend:?}] {out}"
        );
        let text = out["stmts"][0]["text"].as_str().unwrap_or("");
        // HAVING 引用聚合别名 → 必须重复聚合表达式（PG 不允许 HAVING 用 SELECT 别名）
        assert!(text.contains("HAVING COUNT(*) >"), "[{backend:?}] {text}");
        assert!(text.contains("ORDER BY SUM("), "[{backend:?}] {text}");
        assert!(text.contains("DESC"), "[{backend:?}] {text}");
        assert!(text.contains("LIMIT"), "[{backend:?}] {text}");
    }
    assert_sql_parity("aggregate-group-having", &cmd);
}

/// `$having` / 分组后 `$sort` 引用 **by 键**：core 产出的 pipeline 是 Mongo 形态
/// （by 键已被改写为分组 `_id` / `_id.<key>`），SQL 侧须反向映射回分组键表达式 →
/// 「Mongo 支持 ⇒ SQL 同样支持」，不得落到「未映射列」显式 Err。
#[test]
fn dialect_group_having_sort_by_key_is_supported() {
    let registry = registry_with(&schemas());
    let cmd = json!({ "kind": "aggregate", "collection": "posts", "pipeline": [
        { "$group": { "_id": "$status", "n": { "$sum": 1 } } },
        { "$match": { "_id": { "$eq": "published" } } },
        { "$sort": { "_id": 1 } },
        { "$project": { "_id": 0, "status": "$_id", "n": 1 } }
    ] });

    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, &cmd, &registry).expect("group having by-key translate");
        assert_eq!(
            out["unsupported"].as_array().map(|a| a.len()),
            Some(0),
            "[{backend:?}] {out}"
        );
        let text = out["stmts"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("HAVING t."),
            "[{backend:?}] HAVING 应引用分组键列: {text}"
        );
        assert!(
            text.contains("ORDER BY t."),
            "[{backend:?}] ORDER BY 应引用分组键列: {text}"
        );
    }
    assert_sql_parity("aggregate-group-having-by-key", &cmd);
}

/// 全表单组（`by` 省略）→ 无 `GROUP BY`；Mongo 空集护栏 `$facet`/`$replaceRoot` 对 SQL 为 no-op。
#[test]
fn dialect_aggregate_group_all_rows() {
    let registry = registry_with(&schemas());
    let cmd = json!({ "kind": "aggregate", "collection": "posts", "pipeline": [
        { "$group": { "_id": null, "n": { "$sum": 1 }, "total": { "$sum": "$views" } } },
        { "$facet": { "__rows": [] } },
        { "$replaceRoot": { "newRoot": { "$ifNull": [
            { "$arrayElemAt": ["$__rows", 0] },
            { "_id": null, "n": 0, "total": null }
        ] } } },
        { "$project": { "_id": 0, "n": 1, "total": 1 } }
    ] });

    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, &cmd, &registry).expect("group all-rows translate");
        let text = out["stmts"][0]["text"].as_str().unwrap_or("");
        assert!(
            !text.to_uppercase().contains("GROUP BY"),
            "[{backend:?}] 全表单组不应有 GROUP BY: {text}"
        );
        assert!(text.contains("COUNT(*)"), "[{backend:?}] {text}");
    }
    assert_sql_parity("aggregate-group-all-rows", &cmd);
}

/// 分组 by 键为 object 点号路径：SQL 关系映射未把 object 子字段落为列 → 显式报错（绝不静默）。
/// 注：`by` 含关系名由 pipeline 规划层拒绝（见 fixtures/pipeline `error-group-by-relation`）。
#[test]
fn dialect_group_object_dotted_key_is_rejected() {
    let registry = registry_with(&[json!({
        "name": "Course", "collection": "courses", "timestamps": false,
        "fields": {
            "status": { "type": "string" },
            "meta": { "type": "object", "fields": { "level": { "type": "string" } } }
        },
        "relations": {}
    })]);
    let cmd = json!({ "kind": "aggregate", "collection": "courses", "pipeline": [
        { "$group": { "_id": { "meta": { "level": "$meta.level" } },
                      "n": { "$sum": 1 } } },
        { "$project": { "_id": 0, "meta.level": "$_id.meta.level", "n": 1 } }
    ] });
    let err = translate(Backend::Postgres, &cmd, &registry)
        .expect_err("object 点号路径分组键在 SQL 侧必须显式报错");
    assert!(err.contains("meta.level"), "错误应指明分组键: {err}");
}

/// 分组结果行还原：by 键点号路径 → 嵌套对象（非关系列，不得塑形为数组）。
#[test]
fn dialect_group_dotted_key_restores_nested_object() {
    let shape = json!({
        "columns": [
            { "alias": "b0", "path": ["status"],      "isArray": false, "always": true },
            { "alias": "b1", "path": ["meta","level"], "isArray": false, "always": true },
            { "alias": "a0", "path": ["n"],            "isArray": false, "always": true }
        ],
        "present": ""
    });
    let rows = json!([
        { "b0": "draft", "b1": "gold", "a0": 2 },
        { "b0": "draft", "b1": null,   "a0": 5 },
    ]);
    let restored = restore_rows_json(&shape, &rows).expect("restore");
    let arr = restored.as_array().unwrap();
    assert_eq!(arr.len(), 2, "每行一篇: {restored}");
    assert_eq!(
        arr[0].get("meta").and_then(|m| m.get("level")),
        Some(&json!("gold")),
        "点号路径应还原为嵌套对象: {restored}"
    );
    assert_eq!(
        arr[1].get("meta").and_then(|m| m.get("level")),
        Some(&Value::Null),
        "by 键为 null 时也应输出该键（always）: {restored}"
    );
}

// ─── §9.6 关系聚合谓词（跨表条件过滤 / semi-join）的 SQL 下推 ─────────────

/// 用 core 规划出的 Mongo pipeline 驱动 SQL 翻译（与 Mongo 侧逐一对应，端到端一致）
fn rp_cmd(model: &str, collection: &str, c0: Value) -> Value {
    let registry = registry_with(&schemas());
    let mut ast = parse_gql(&format!("{model}($condition:@c0){{ _id }}")).expect("GQL 解析失败");
    let mut params = Map::new();
    params.insert("c0".to_string(), c0);
    let pipeline = build_pipeline(&mut ast, &params, &registry, None).expect("build_pipeline");
    json!({ "kind": "aggregate", "collection": collection, "pipeline": pipeline })
}

/// `$count > N` → `EXISTS (... GROUP BY fk HAVING COUNT(*) > ?)`；跨后端归一一致。
#[test]
fn dialect_relation_predicate_count_exists() {
    let cmd = rp_cmd(
        "Order",
        "orders",
        json!({ "items": { "$count": { "$gt": 3 } } }),
    );
    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, &cmd, &registry_with(&schemas())).expect("translate");
        assert_eq!(
            out["unsupported"].as_array().map(|a| a.len()),
            Some(0),
            "[{backend:?}] 关系谓词应完全下推: {out}"
        );
        let text = out["stmts"][0]["text"].as_str().unwrap_or("");
        let norm = normalize(text);
        assert!(
            norm.contains("exists (select 1 from order_items c where c.orderid = t._id"),
            "[{backend:?}] 应为 EXISTS 相关子查询: {text}"
        );
        assert!(
            norm.contains("group by c.orderid having count(*) > ?"),
            "[{backend:?}] 应为 GROUP BY + HAVING: {text}"
        );
        assert!(
            !norm.contains("join"),
            "[{backend:?}] semi-join 不得扇出为 JOIN: {text}"
        );
        let params = out["stmts"][0]["params"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(params, vec![json!(3)], "[{backend:?}] {out}");
    }
}

/// 子 `filter` + `$sum` having → `WHERE … AND … GROUP BY … HAVING SUM(…) > ?`
#[test]
fn dialect_relation_predicate_filter_sum_having() {
    let cmd = rp_cmd(
        "Order",
        "orders",
        json!({ "items": {
            "filter": { "sku": "x" },
            "agg": { "s": { "$sum": "qty" } },
            "having": { "s": { "$gt": 10 } }
        } }),
    );
    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, &cmd, &registry_with(&schemas())).expect("translate");
        let text = out["stmts"][0]["text"].as_str().unwrap_or("");
        let norm = normalize(text);
        assert!(
            norm.contains("c.sku = ?"),
            "[{backend:?}] 子 filter 应下推为子查询 WHERE: {text}"
        );
        assert!(
            norm.contains("having sum(c.qty) > ?"),
            "[{backend:?}] having 应为 HAVING SUM: {text}"
        );
        let params = out["stmts"][0]["params"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(params, vec![json!("x"), json!(10)], "[{backend:?}] {out}");
    }
}

/// anti-join：`$not` 包裹关系谓词 → `NOT EXISTS`；`$exists:false` 同义。
#[test]
fn dialect_relation_predicate_anti_join_not_exists() {
    for c0 in [
        json!({ "$not": { "items": { "$count": { "$gt": 3 } } } }),
        json!({ "items": { "$exists": false } }),
    ] {
        let cmd = rp_cmd("Order", "orders", c0.clone());
        for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
            let out = translate(backend, &cmd, &registry_with(&schemas())).expect("translate");
            let text = out["stmts"][0]["text"].as_str().unwrap_or("");
            let norm = normalize(text);
            assert!(
                norm.contains("not exists (select 1 from order_items c"),
                "[{backend:?}] 应为 NOT EXISTS: {text}"
            );
            assert!(
                !norm.contains("not not exists"),
                "[{backend:?}] 不得双重否定: {text}"
            );
        }
    }
}

/// 标量条件 + 关系谓词并存：AND 合并、参数顺序与文本一致。
#[test]
fn dialect_relation_predicate_with_scalar_sibling() {
    let cmd = rp_cmd(
        "Order",
        "orders",
        json!({ "$and": [ { "code": "A" }, { "items": { "$exists": true } } ] }),
    );
    let out = translate(Backend::Postgres, &cmd, &registry_with(&schemas())).expect("translate");
    let text = out["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("t.\"code\" = $1") && text.contains("EXISTS (SELECT 1"),
        "标量条件与 EXISTS 应 AND 并存: {text}"
    );
    assert_eq!(
        out["stmts"][0]["params"],
        json!(["A", 0]),
        "标量参数在前、EXISTS 的 HAVING 参数在后: {out}"
    );
}

// ─── §9.7 布尔归一 / PG 浮点字面量类型标注（场景矩阵暴露的两个跨后端缺口）──

/// §9.7「布尔归一」：schema 的 `boolean` 字段在 SQL 侧存为 `0/1`（MySQL `TINYINT(1)`、
/// SQLite `INTEGER`）→ 行还原必须归一为 JSON `bool`，与 PostgreSQL 原生 `BOOLEAN`、
/// MongoDB 的 `true/false` 类型一致（否则同一逻辑字段跨后端类型漂移）。
#[test]
fn dialect_boolean_column_restores_as_json_bool() {
    // `paid` 用规范拼写 `boolean`，`free` 用简写 `bool`（示例 schema 的实际写法）—— 二者同等对待
    let registry = registry_with(&[json!({
        "name": "Enrollment", "collection": "enrollments", "timestamps": false,
        "fields": {
            "paid": { "type": "boolean" }, "free": { "type": "bool" },
            "amount": { "type": "float" }
        },
        "relations": {}
    })]);
    let cmd = json!({ "kind": "find", "collection": "enrollments", "filter": {},
                      "projection": { "_id": 1, "paid": 1, "free": 1, "amount": 1 } });

    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, &cmd, &registry).expect("translate");
        let shape = &out["stmts"][0]["rowShape"];
        let cols = shape["columns"].as_array().expect("columns");
        for bool_col in ["paid", "free"] {
            let c = cols
                .iter()
                .find(|c| c["path"] == json!([bool_col]))
                .unwrap_or_else(|| panic!("{bool_col} 列"));
            assert_eq!(
                c["bool"],
                json!(true),
                "[{backend:?}] schema 布尔字段必须标记（含 `bool` 简写）: {shape}"
            );
        }
        let amount = cols
            .iter()
            .find(|c| c["path"] == json!(["amount"]))
            .expect("amount 列");
        assert_eq!(
            amount["bool"],
            json!(false),
            "[{backend:?}] 非布尔字段不得被标记: {shape}"
        );
    }

    // 还原：MySQL/SQLite 的 0/1 → true/false；非布尔列原样透传（不误伤）
    let shape = &translate(Backend::Sqlite, &cmd, &registry).expect("translate")["stmts"][0]
        ["rowShape"];
    let rows = json!([
        { "_id": "e1", "paid": 1,    "free": 0,    "amount": 3.5, "__present": ",paid,free,amount," },
        { "_id": "e2", "paid": 0,    "free": 1,    "amount": 4,   "__present": ",paid,free,amount," },
        { "_id": "e3", "paid": null, "free": null, "amount": 0,   "__present": ",paid,free,amount," },
    ]);
    let restored = restore_rows_json(shape, &rows).expect("restore");
    assert_eq!(
        restored,
        json!([
            { "_id": "e1", "paid": true,  "free": false, "amount": 3.5 },
            { "_id": "e2", "paid": false, "free": true,  "amount": 4 },
            { "_id": "e3", "paid": null,  "free": null,  "amount": 0 },
        ]),
        "0/1 应归一为 true/false，null 保持 null，非布尔列不变: {restored}"
    );

    // PG 原生 BOOLEAN → 已是 bool，归一为 no-op
    let pg_shape = &translate(Backend::Postgres, &cmd, &registry).expect("translate")["stmts"][0]
        ["rowShape"];
    let pg_rows = json!([{ "_id": "e1", "paid": true, "free": false, "amount": 3.5 }]);
    let pg_restored = restore_rows_json(pg_shape, &pg_rows).expect("restore");
    assert_eq!(
        pg_restored,
        json!([{ "_id": "e1", "paid": true, "free": false, "amount": 3.5 }])
    );
}

/// D1 回归：PostgreSQL 由**服务端按上下文**推断 `$n` 类型 —— `int_col > $1`（值 `2.5`）
/// 会被推断为 `integer`，执行期报 `invalid input syntax for type integer: "2.5"`。
/// core 对 PG 的非整数字面量显式标注 `CAST($n AS double precision)`，与 py 宿主 asyncpg
/// 「Python float → float8」一致；MySQL/SQLite 与整数字面量不受影响。
#[test]
fn dialect_postgres_float_literal_gets_explicit_double_cast() {
    let registry = registry_with(&schemas());

    let float_cmd = json!({ "kind": "find", "collection": "posts",
                            "filter": { "views": { "$gt": 2.5 } }, "projection": { "_id": 1 } });
    let pg = translate(Backend::Postgres, &float_cmd, &registry).expect("translate");
    let text = pg["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("CAST($1 AS double precision)"),
        "PG 浮点字面量须显式标注双精度: {text}"
    );

    // 整数字面量不加 CAST（既有 SQL 与跨后端对拍保持原样）
    let int_cmd = json!({ "kind": "find", "collection": "posts",
                          "filter": { "views": { "$gte": 10 } }, "projection": { "_id": 1 } });
    let pg_int = translate(Backend::Postgres, &int_cmd, &registry).expect("translate");
    assert!(
        !pg_int["stmts"][0]["text"].as_str().unwrap_or("").contains("CAST("),
        "整数字面量不应加 CAST: {}",
        pg_int["stmts"][0]["text"]
    );

    // MySQL / SQLite 保持 `?` 占位（动态类型，无需 CAST）
    for backend in [Backend::Mysql, Backend::Sqlite] {
        let out = translate(backend, &float_cmd, &registry).expect("translate");
        assert!(
            !out["stmts"][0]["text"].as_str().unwrap_or("").contains("CAST("),
            "[{backend:?}] 不应引入 CAST: {}",
            out["stmts"][0]["text"]
        );
    }

    // `$in` 内的浮点元素同样标注（整数元素不标注）
    let in_cmd = json!({ "kind": "find", "collection": "posts",
                         "filter": { "views": { "$in": [2.5, 3] } }, "projection": { "_id": 1 } });
    let pg_in = translate(Backend::Postgres, &in_cmd, &registry).expect("translate");
    let in_text = pg_in["stmts"][0]["text"].as_str().unwrap_or("");
    assert!(
        in_text.contains("CAST($1 AS double precision)") && in_text.contains("$2"),
        "$in 中浮点元素须标注、整数元素保持: {in_text}"
    );
    assert_eq!(
        pg_in["stmts"][0]["params"],
        json!([2.5, 3]),
        "参数值本身不变: {pg_in}"
    );
}
