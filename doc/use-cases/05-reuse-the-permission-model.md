# Reuse the permission model

## The problem

You are embedding `rust-store-core` in your own Rust service and you need ownership
and role checks on every read and write. The usual answer is to re-implement them in
the service — and that is the wrong one. The schema already declares the `read` /
`write` whitelists, so a second implementation grows beside the first, drifts from it,
and becomes the place where an ownership rule is quietly forgotten.

## Why rust-store

The permission engine is part of the core, not of the hosts. The rules are declared in
the schema (`read` / `write`, `field.read` / `field.write`, `relation.read`,
`compute.read`) and evaluated *while a plan is built*, so the command you receive is
already filtered:

- a read projection contains only the fields the caller may read;
- an owner-scoped read (`read: ["creator"]`) gets a `createdBy` condition injected into
  its filter;
- a write the caller may not perform is rejected before any command is produced.

Two properties matter when reusing it:

- **The context is explicit.** `ctx: Option<&Context>` is a parameter on every plan
  entry point. There is no ambient or async-local context to install.
- **Fail-secure is opt-in.** `Registry::set_require_context(true)` turns a missing
  context into a hard error; it defaults to off (fail-open) for parity with the original
  implementation.

## Walkthrough

```rust
use serde_json::{json, Map, Value};

use rust_store_core::command::{apply_route_override, plan_query};
use rust_store_core::permission::{
    can_read_schema, can_write_schema, evaluate, merge_owner_condition,
    should_inject_owner_condition, Context, Doc,
};
use rust_store_core::schema::Registry;

fn main() -> Result<(), String> {
    let mut registry = Registry::new();
    registry.register(&json!({
        "name": "Invoice",
        "collection": "invoices",
        "read": ["creator"],                 // only the document's creator may read it
        "write": ["admin", "creator"],
        "fields": {
            "code":   { "type": "string" },
            "amount": { "type": "float" }
        },
        "relations": {}
    }))?;

    // 1) Fail-secure: every plan entry point now rejects `ctx = None` with the
    //    `ERR_NO_CONTEXT:` sentinel instead of treating it as a system call.
    registry.set_require_context(true);

    // 2) The host builds this from its own authentication result. The core trusts
    //    `userId` / `roles` — it does not verify who the caller is.
    let user = Context {
        user_id: Some("u_42".to_string()),
        roles: Some(vec!["user".to_string()]),
        role: None,
        internal: false,
    };

    // 3) Planning performs the checks. For this ctx the planned filter becomes
    //    { "$and": [ { "code": "A-1" }, { "createdBy": "u_42" } ] }.
    let mut params: Map<String, Value> = Map::new();
    params.insert("c0".to_string(), json!({ "code": "A-1" }));
    let plan = plan_query(
        "Invoice($condition:@c0){ code, amount }",
        &params,
        &registry,
        Some(&user),
    )?;

    // 4) With `require_context` on, a missing context is an explicit failure.
    let denied = plan_query("Invoice{code}", &Map::new(), &registry, None);
    assert!(denied.unwrap_err().starts_with("ERR_NO_CONTEXT:"));

    // 5) The same decisions are public functions, for code outside a plan call.
    let schema = registry.get("Invoice")?;
    assert!(can_read_schema(schema, Some(&user)));
    assert!(can_write_schema(schema, Some(&user)));      // write: ["admin","creator"]
    assert!(should_inject_owner_condition(schema, Some(&user)));
    let _owned: Option<Value> =
        merge_owner_condition(schema, Some(&user), Some(json!({ "code": "A-1" })));
    // `creator` is resolved from the document: `Missing` = an insert (no doc yet) passes.
    let _insert_allowed = evaluate(Some(&user), schema.read.as_deref(), Doc::Missing);

    // 6) Internal maintenance paths use an explicit system context, never `None`.
    let internal = Context::system();
    assert!(can_write_schema(schema, Some(&internal)));

    // 7) Routing is orthogonal to permissions — and is trusted server-side input.
    let mut plan_value = plan.to_value();
    apply_route_override(&mut plan_value, &json!({ "namespace": "tenant_42" }));

    Ok(())
}
```

The check runs *inside* planning, in this order (`plan_query_ast_mut`):

1. `ensure_context` — refuse `ctx = None` when `require_context` is on.
2. `registry.get(&ast.model)` — the schema must be registered.
3. `can_read_schema` — schema-level `read` whitelist; denial is `ERR_PERMISSION:`.
4. `merge_owner_condition` — inject `createdBy` for a `creator`-only read (also when the
   query carries no `$condition` at all, so it cannot fall back to the whole table).
5. `check_readable_relations` + `build_pipeline` — relation-level `read` and the target
   schema's `read` are checked; projection pruning (`get_readable_fields`) then drops
   unreadable fields.

The write path has the same shape: `plan_insert` runs `ensure_context` →
`can_write_schema` → `build_insert_doc`, whose `filter_writable_data` keeps only writable
fields. `plan_update` / `plan_remove` run `ensure_context` → `check_write_perm`, which
rejects `guest` and non-writers outright and, for a `write: ["creator"]` schema, returns a
probe command (`{"needsProbe": …}`) that the host executes before re-entering.

| The engine guarantees | The engine does not guarantee |
| --- | --- |
| a plan whose commands are filtered by the schema's schema-level / field / relation / computed-column rules | that `ctx` is truthful — `userId` and `roles` are trusted host input |
| an explicit `ERR_PERMISSION:` error for an unreadable schema, relation or computed-column dependency | that an unreadable *field* errors — it is silently removed from the projection |
| `ERR_NO_CONTEXT:` for a missing context once `require_context` is on | authentication, sessions or tokens — this is not an authentication system |
| an injected `createdBy` condition for a `creator`-only read | that the produced command was executed correctly — the engine never runs it |

In short: the engine produces a **filtered plan**, and whoever supplies `Context` decides
who the user is.

## Pitfalls

- **It is not an authentication system.** `Context` is data you pass in, not proof of
  identity. Resolve the user in your service first, then build the context from that.
- **Fail-open is the default.** `require_context` defaults to off; if you forget `ctx`,
  permission checks are skipped rather than denied. Call
  `Registry::set_require_context(true)` at startup and pass `Context::system()` only for
  genuinely internal calls.
- **`routeOverride` / `route_override` is trusted input.** It rewrites the
  `source` / `namespace` location triple on every command body after planning
  (`apply_route_override`), while permission, computed-column and field validation keep
  running against the *structural* schema and are explicitly orthogonal to routing. If
  the override is fed from user input, a caller can point a command at a source or
  namespace the schema never authorised — an authorization-bypass surface (CWE-639).
  Build it from tenant configuration, never from a request field.
- **Field pruning is silent; relation denial is not.** A field outside the caller's
  `field.read` set is dropped from the projection, whereas an unreadable relation (or a
  computed column whose dependency is unreadable) raises. Do not treat a missing field in
  the projection as "no such field".
- **Error prefixes are a stable contract.** All permission denials carry
  `ERR_PERM_PREFIX` (`ERR_PERMISSION:`; the concrete sentinels are `ERR_PERMISSION`,
  `ERR_NO_WRITE`, `ERR_NO_DELETE`, `ERR_NO_BATCH_WRITE`, and `ERR_NO_CONTEXT` for the
  missing-context case, in `core/src/command/mod.rs`). Match on the prefix and strip it —
  the human-readable text is free to change.
- **The engine returns a plan, not a result.** Permission filtering happens at plan time;
  it only holds if the host executes exactly the commands it was given.

## See also

- [README — Permission model](../../README.md#permission-model)
- [README — Boundaries and gotchas](../../README.md#boundaries-and-gotchas)
- [README — API reference](../../README.md#api-reference)
- [Embed the core in a Rust service](01-embed-the-core-in-a-rust-service.md)
- [SQL pushdown limits by dialect](06-sql-pushdown-limits-by-dialect.md)
