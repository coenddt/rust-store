//! dialect 对拍测试（Mongo 命令 → 关系型 SQL）
//!
//! 核心 parity 性质：同一命令在 MySQL / PostgreSQL / SQLite 三端，参数化 SQL 在归一化后
//! 语义一致（只差标识符引号与占位符风格）；`restore_rows`/`introspect`/`overlay` 为纯逻辑，
//! 四侧可复现。此测试把输入以 JSON 内联，作断言基准（无外部 golden；原 JS 参考实现已退役，
//! 其黄金基准为冻结快照，复算校验见 `node tools/verify-fixtures.js`）。

use serde_json::{json, Value};

use rust_store_core::dialect::{
    introspect_to_schema_json, merge_schema, restore_rows_json, translate, Backend,
};
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

/// `$lookup` 子 `$limit`（每父 top-N，需 LATERAL/窗口函数）→ 标记 `childLimit`，
/// 且**不得**下推该 JOIN（否则会静默返回未截断的子集）。
#[test]
fn dialect_translate_child_limit_marked_unsupported() {
    let registry = registry_with(&schemas());
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

        let unsupported = out
            .get("unsupported")
            .and_then(|v| v.as_array())
            .expect("unsupported 数组");
        assert_eq!(
            unsupported.len(),
            1,
            "[{:?}] 应恰一条 unsupported: {}",
            backend,
            out
        );
        assert_eq!(
            unsupported[0].get("code").and_then(|v| v.as_str()),
            Some("childLimit"),
            "[{:?}] code 应为 childLimit: {}",
            backend,
            out
        );
        // 不得包含对该子表的 JOIN（否则会返回未截断的子集）
        let text = out["stmts"][0]["text"].as_str().unwrap_or("");
        assert!(
            !text.contains("JOIN"),
            "[{:?}] 不应下推子 limit 的 JOIN: {}",
            backend,
            text
        );
        // 警告必须同时给出（Host 可读）
        assert!(
            !out.get("warnings")
                .and_then(|v| v.as_array())
                .unwrap_or(&vec![])
                .is_empty(),
            "[{:?}] 应同时给 warning: {}",
            backend,
            out
        );
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
