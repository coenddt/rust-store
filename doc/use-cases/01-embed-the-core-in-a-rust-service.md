# Embed the core in a Rust service

## The problem

You have a Rust service and you want the same query dialect, permission model and
computed-column behaviour that the `nodejs-store` / `py-store` hosts already use. You do
not want to re-implement GQL parsing, permission checks or SQL translation, and you do not
want to hand the engine a database connection — your service already owns its drivers and
its connection pool.

## Why rust-store

`rust-store-core` is a plain Rust library that holds all of the pure logic: schema registry,
GQL parsing, permission checks, computed columns, command planning and dialect translation.
It performs **no IO** — it never opens a connection, never reads the clock and never
generates a random id. Every plan method returns command JSON that *you* execute.

The crates are `publish = false`, so you depend on the core **by path** rather than through
crates.io.

## Walkthrough

Add the core as a path dependency:

```toml
[dependencies]
rust-store-core = { path = "path/to/rust-store/core" }
serde_json = "1"
```

Register schemas, plan a read, then translate the command for a relational backend:

```rust
use serde_json::{json, Map};

use rust_store_core::command::{plan_insert, plan_query};
use rust_store_core::dialect::{translate, Backend};
use rust_store_core::permission::Context;
use rust_store_core::schema::Registry;

fn main() -> Result<(), String> {
    // 1) Register schemas. The core derives the `<Name>Deleted` archive table for you,
    //    and validates the `(source, namespace, collection)` location triple.
    let mut registry = Registry::new();
    registry.register(&json!({
        "name": "Order",
        "collection": "orders",
        "timestamps": false,
        "fields": {
            "code":   { "type": "string" },
            "amount": { "type": "float" }
        },
        "relations": {}
    }))?;

    // 2) Plan a read. `plan_query` is a pure `gql -> plan` function: it does not touch a
    //    database. `ctx` is an explicit parameter.
    let mut params = Map::new();
    params.insert("c0".to_string(), json!({ "amount": { "$gte": 10 } }));

    let ctx = Context::system(); // internal call: permission engine passes everything
    let plan = plan_query(
        "Order($condition:@c0){ code, amount }",
        &params,
        &registry,
        Some(&ctx),
    )?;

    // `plan.commands` is MongoDB command JSON. Hand it to YOUR driver: the `source`,
    // `namespace` and `collection` fields tell you which connection to use.
    let cmd = plan.commands.first().ok_or("query planned no command")?;
    // let docs = my_driver.execute(cmd)?;

    // 3) The very same command translates to parameterized SQL via a pure function.
    //    The result is `{ backend, stmts, warnings, unsupported }`.
    let sql = translate(Backend::Postgres, cmd, &registry)?;
    let _ = sql;

    // 4) Writes need a clock and an id source. The core has neither, so the host supplies
    //    both — this is what makes results deterministic and cross-language reproducible.
    let now_ms: i64 = 1_760_000_000_000; // your host's clock
    let new_id = "ord_01HZ";             // your host's id generator
    let write = plan_insert(
        "Order",
        &registry,
        Some(&ctx),
        &json!({ "code": "A-1", "amount": 12.5 }),
        now_ms,
        new_id,
        None, // sync computed-column callbacks, if any
    )?;
    // write["command"] = insertOne command; write["returns"] = the doc after defaults/computes.

    Ok(())
}
```

The binding name for the same translation entry point is `dialectTranslate` / `dialect_translate`;
in Rust it is exported as `rust_store_core::dialect::translate`.

## Pitfalls

- **Path dependency only.** The three workspace crates are `publish = false`; there is
  nothing to fetch from crates.io.
- **The core does no IO.** `plan.commands` is inert JSON. Your service must execute it and,
  for two-phase plans, substitute the `PHASE1_IDS` placeholder and call
  `restore_sort_order` afterwards.
- **No clock, no randomness.** `now` and `newId(s)` are always passed in by the caller.
- **`ctx` is explicit.** Omitting it means "no context" (fail-open by default). Turn on
  `Registry::set_require_context(true)` for fail-secure behaviour and pass
  `Context::system()` only for genuine internal calls.
- **Errors are strings with stable prefixes.** Permission denials carry the
  `ERR_PERMISSION:` prefix (`ERR_PERM_PREFIX`, `ERR_NO_WRITE`, `ERR_NO_BATCH_WRITE`,
  `ERR_NO_CONTEXT` live in `core/src/command/mod.rs`); map them to your own error types.
- **The query string is GQL, not SQL.** `Order($condition:@c0){ code, amount }` is compiled
  by the core; SQL only appears after `translate`.

## See also

- [README — API reference](../../README.md#api-reference)
- [README — Quick start](../../README.md#quick-start)
- [One dialect across four databases](03-one-dialect-across-four-databases.md)
- [Node.js / Python parity](04-node-python-parity.md)
