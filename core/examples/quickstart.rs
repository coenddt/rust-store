//! Quickstart — register a schema, plan a GQL query, translate the plan to SQL.
//!
//! Run with `cargo run -p rust-store-core --example quickstart`.
//!
//! Nothing in this file opens a connection: `plan_query` and `translate` are pure
//! functions, which is what makes the core reusable from any host.

use serde_json::{json, Map};

use rust_store_core::command::plan_query;
use rust_store_core::dialect::{translate, Backend};
use rust_store_core::schema::Registry;

fn main() -> Result<(), String> {
    // 1. A model is plain JSON — defined once, usable against every backend.
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

    // 2. GQL in, MongoDB command JSON out. Values are referenced by `@key`.
    let gql = "Post($condition:@c0,$sort:@s1,$limit:@l0){title, status, views}";
    let mut params = Map::new();
    params.insert("c0".to_string(), json!({ "status": "published" }));
    params.insert("s1".to_string(), json!({ "views": -1 }));
    params.insert("l0".to_string(), json!(20));

    let plan = plan_query(gql, &params, &registry, None)?;

    println!("collection = {}", plan.collection);
    println!("mode       = {}", plan.mode.as_str());

    for cmd in &plan.commands {
        println!("command    = {}", serde_json::to_string(cmd).unwrap());

        // 3. The host runs `cmd` against MongoDB — or, for a SQL backend, asks the
        //    core for parameterized SQL and binds it with its own driver.
        let sql = translate(Backend::Postgres, cmd, &registry)?;
        println!("sql        = {}", serde_json::to_string(&sql).unwrap());
    }

    Ok(())
}
