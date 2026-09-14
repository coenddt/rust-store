# rust-store-core

**One data engine for MongoDB, MySQL, PostgreSQL and SQLite.** A MongoDB-style query language (GQL), permission checks, computed columns, command planning and SQL dialect translation — as a plain Rust library that **opens no connection, starts no clock and generates no randomness**.

`rust-store-core` is the engine behind [`rust-store-node`](https://www.npmjs.com/package/rust-store-node) (napi-rs) and [`rust-store-py`](https://pypi.org/project/rust-store-py/) (PyO3), and through them behind [`nodejs-store`](https://www.npmjs.com/package/nodejs-store) and [`py-store`](https://pypi.org/project/storepy).

## Install

```bash
cargo add rust-store-core
```

## What it does

| Step | Function | Input → output |
| --- | --- | --- |
| Parse + plan | `command::plan_query` | GQL + params + registry + context → `QueryPlan` (MongoDB command JSON) |
| Translate | `dialect::translate` | command JSON + `Backend` → parameterized SQL statements |
| Rehydrate | `dialect::restore_rows_json` | flat JOIN rows → nested documents |

The host is the only IO boundary: it takes the planned command, runs it against its own driver, and hands the rows back for rehydration.

## Three guarantees

1. **No IO, no clock, no randomness.** `now` and `new_ids` are always passed in by the caller. This is what makes the engine deterministic and reproducible across languages.
2. **Explicit over silent.** Anything that cannot be translated safely returns an error or an `unsupported` marker plus warnings. The engine never emits SQL that is quietly missing a clause.
3. **JSON in, JSON out.** The Node.js and Python bindings only convert and forward, so the two hosts cannot drift apart semantically.

## Quickstart

```rust
use serde_json::{json, Map};
use rust_store_core::command::plan_query;
use rust_store_core::dialect::{translate, Backend};
use rust_store_core::schema::Registry;

let mut registry = Registry::new();
registry.register(&json!({
    "name": "Post",
    "collection": "posts",
    "timestamps": true,
    "fields": {
        "title":  { "type": "string" },
        "status": { "type": "string", "default": "draft" },
        "views":  { "type": "int" }
    },
    "relations": {}
}))?;

let gql = "Post($condition:@c0,$sort:@s1,$limit:@l0){title, status, views}";
let mut params = Map::new();
params.insert("c0".to_string(), json!({ "status": "published" }));
params.insert("s1".to_string(), json!({ "views": -1 }));
params.insert("l0".to_string(), json!(20));

let plan = plan_query(gql, &params, &registry, None)?;

for cmd in &plan.commands {
    // The host either executes `cmd` against MongoDB …
    let sql = translate(Backend::Postgres, cmd, &registry)?;
    // … or binds this parameterized SQL with its own driver.
    println!("{}", serde_json::to_string(&sql)?);
}
```

A runnable version lives in [`examples/quickstart.rs`](examples/quickstart.rs) — `cargo run --example quickstart`.

## GQL in one look

```text
ModelName($condition:@c0,$sort:@s1,$skip:@sk,$limit:@l1) {
  field1, field2, obj.subField,
  RelationName($condition:@c2,$sort:@s3,$limit:@l2) { field3, NestedRelation { field4 } }
}
```

Values are referenced from the params object by `@key`. Relations are declared in the schema and compiled by the engine into `$lookup` / `$addFields` — you never hand-write `$lookup`. Root-level `$group` / `$having` are supported, with the same operator whitelist (`$count` / `$sum` / `$avg` / `$min` / `$max`) on both the Mongo and the SQL side.

## Documentation

- **Full documentation site** — <https://coenddt.github.io/rust-store/> — scenario walkthroughs with runnable code and the engine's exact limits, one indexable page per scenario.
- **API reference** — <https://docs.rs/rust-store-core>
- **Repository** — <https://github.com/coenddt/rust-store>

## License

MIT
