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

## Naming & translation

Canonicalization has a single implementation in `core::naming`, re-exported by `core-node` / `core-py`; hosts must not re-implement it.

<!-- SPEC:NAMING-STYLE:BEGIN -->
### Naming: freeform definitions, system-directed translation

Definitions (`collection`, fields, referenced relation fields, computed-column keys, `fnRef` values, index names) may use any style; the engine translates them to the target style. Contract keys (`fnRef`, `localField`, `foreignField`, `asyncFn`, `type`, ...) and the schema `name` are never translated.

| Target | Style | Example (`orderTotal`) |
|---|---|---|
| MySQL / PostgreSQL / SQLite (physical) | snake_case | `order_total` |
| MongoDB (physical) | camelCase | `orderTotal` |
| Node.js / Java / C# / Rust (code; computed columns follow) | camelCase | `orderTotal` |
| Go (code; computed columns follow) | PascalCase (must be exported) | `OrderTotal` |
| Python (code; computed columns follow) | snake_case | `order_total` |

Canonicalization (single implementation `core::naming`, re-exported by the bindings; hosts must not re-implement it): split on `_`, `-`, `.`, space and at lower/digit-to-upper boundaries; a trailing uppercase in a run followed by a lowercase starts the next token (`HTTPServer` -> `[http, server]`, `userID` -> `[user, id]`); digits stay inside a token (`order2Items` -> `[order2, items]`). Reassembly: snake = `t1_t2`, camel = `t1T2`, pascal = `T1T2`.

Two logical names in one schema that canonicalize equal (`orderTotal` vs `order_total`), or a name that canonicalizes onto a reserved contract key (e.g. `fnref`), is an error `ERR_NAME_CONFLICT:` and the service does not start (never silently overwritten).
<!-- SPEC:NAMING-STYLE:END -->

<!-- SPEC:FNREF:BEGIN -->
### Computed columns: `fnRef` binding by composite name + canonical match

Computed columns live at the schema top level, `computes: { <key>: { type, fn | asyncFn | agg, fnRef?, depends?, read? } }` (`fn` / `asyncFn` / `agg` are mutually exclusive).

- The logical `fnRef` defaults to `<schema.name>.<computed-column key>` (generated, never hand-written); since `name` is globally unique, the `fnRef` is globally unique too.
- Host implementations bind by canonicalization: both the implementation's name in the host language style and the schema's logical `fnRef` are canonicalized to token sequences and compared. So Node's `orderAmountLabel` and Python's `order_amount_label` bind to the same logical computed column.
- Reusing one implementation across schemas: write an explicit shared name (e.g. `"fnRef": "common.moneyLabel"`); naming goes from required to optional.
- Every declared `fnRef` must have an implementation, otherwise the service fails to start with `ERR_FN_MISSING`.
<!-- SPEC:FNREF:END -->

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
