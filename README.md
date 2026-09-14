# rust-store

**A single Rust core engine for multi-backend data access — GQL parsing, permission checks, computed columns, command planning and SQL dialect translation, exposed to Node.js and Python through native bindings.**

![crates.io](https://img.shields.io/crates/v/rust-store-core)
![npm version](https://img.shields.io/npm/v/rust-store-node)
![PyPI version](https://img.shields.io/pypi/v/rust-store-py)
![license](https://img.shields.io/badge/license-MIT-blue)
![rust](https://img.shields.io/badge/rust-stable-orange)
![bindings](https://img.shields.io/badge/bindings-napi--rs%20%7C%20PyO3-blueviolet)

`rust-store` is the engine behind [`nodejs-store`](https://github.com/coenddt/nodejs-store) and [`py-store`](https://github.com/coenddt/py-store). It contains **no database driver and performs no IO**: it turns a query (GQL) or a write into a backend-agnostic **command** (MongoDB command JSON) for its host to execute, and translates those commands into parameterized SQL for MySQL / PostgreSQL / SQLite.

> 中文文档见 [README.zh-CN.md](README.zh-CN.md)。

**Documentation site:** <https://coenddt.github.io/rust-store/> — every scenario walkthrough with runnable code and the engine's exact limits, one indexable page per scenario.

---

## Table of contents

- [What it is](#what-it-is)
- [When to use it](#when-to-use-it)
- [How it relates to nodejs-store and py-store](#how-it-relates-to-nodejs-store-and-py-store)
- [Workspace layout](#workspace-layout)
- [Installation](#installation)
- [Quick start](#quick-start)
- [API reference](#api-reference)
- [GQL capabilities](#gql-capabilities)
- [Backends and dialects](#backends-and-dialects)
- [Permission model](#permission-model)
- [Testing and parity](#testing-and-parity)
- [Boundaries and gotchas](#boundaries-and-gotchas)
- [FAQ](#faq)
- [Related projects](#related-projects)

---

## What it is

A multi-backend data engine written once in Rust and shared by every host language.

- **Query language**: GQL — a MongoDB-flavoured tree syntax with relations, pagination, grouping and aggregate predicates.
- **Planning**: GQL + params → **MongoDB command JSON** (`find` / `aggregate` / `countDocuments` / writes).
- **Translation**: MongoDB command JSON → **parameterized SQL** for MySQL, PostgreSQL and SQLite (pure functions).
- **Result rehydration**: flat JOIN rows → nested documents.
- **Cross-cutting concerns**: schema registry, permission engine (schema / field / relation / computed-column level), computed columns (`fn` / `asyncFn` / `agg`), soft-delete archive planning, federation planning across datasources.

**Core principles** (enforced across the codebase):

1. **No IO, no clock, no randomness.** The core never opens a connection and never reads the system clock. `now` and `newId(s)` are always passed in by the host — this is what makes the engine deterministic and cross-language reproducible.
2. **Explicit over silent.** Anything that cannot be translated safely raises an error or emits a structured `unsupported` + warning. The engine never produces SQL that is quietly missing a clause.
3. **JSON in, JSON out.** Bindings only convert and forward; they add no behaviour, so Node.js and Python cannot drift apart semantically.

### How it differs from specific projects

Positioning only, based on those projects' public documentation at the time of writing — verify against your own requirements.

- **vs `sqlx` / Diesel / SeaORM** — those are Rust database toolkits and ORMs that talk to SQL databases directly. `rust-store-core` never opens a connection: it plans a command and translates dialects, and the resulting command JSON is executed by the Node.js or Python host. That is what lets one engine serve both hosts with identical semantics.
- **vs writing the layer twice** — the usual alternative is a JavaScript implementation plus a Python re-implementation, which drifts over time. Here a single Rust core is bound twice (`napi-rs`, `PyO3`) over JSON-only bridges, and `core/tests/parity*.rs` plus the golden fixtures in `fixtures/` enforce that the two bindings stay identical.
- **vs `transports`-style "one core, several bindings" projects** — sharing a Rust core across bindings is a proven pattern for serialization/transport layers. `rust-store` applies it to *data access semantics*: one GQL, one permission engine and four SQL/Mongo dialects behind two language bindings.
- **vs doing it in the host language** — implementing GQL parsing, permissions and four SQL dialects in JavaScript *and* Python means two code paths, two bug surfaces and two sets of edge cases. The Rust core makes the boundary explicit: pure logic in one place, IO in the hosts.

## When to use it

- **You are building the Node.js or Python hosts** ([`nodejs-store`](https://github.com/coenddt/nodejs-store) / [`py-store`](https://github.com/coenddt/py-store)) or debugging them — this repo is where GQL parsing, permissions and dialect translation actually live.
- **You want the same query dialect in a Rust service.** `rust-store-core` is a plain Rust library: register schemas, plan a query, translate to SQL, hand the command to your own driver.
- **You are building a host for another language.** The core is language-agnostic; `core-node` (napi-rs) and `core-py` (PyO3) are two worked examples of the JSON command contract.
- **You need guaranteed parity between a Node service and a Python service.** Both consume the identical engine, so a query behaves the same in both.
- **You want permission and computed-column logic out of your application code**, expressed in the schema and enforced at plan time.
- **You are building an AI / natural-language query layer.** Planning is a pure function (`gql → plan`), so a model's output can be compiled, inspected and rejected before anything touches a database.

## How it relates to nodejs-store and py-store

```
                 ┌──────────────────────────────┐
   Node.js  ──▶  │  nodejs-store (npm, host)    │ ─┐
                 └──────────────────────────────┘  │  rust-store-node (napi-rs)
                                                   ▼
                                     ┌───────────────────────────────┐
                                     │ rust-store/core (pure logic)  │
                                     │ GQL · permissions · computes  │
                                     │ command planning · dialects   │
                                     └───────────────────────────────┘
                                                   ▲
                 ┌──────────────────────────────┐  │  rust-store-py (PyO3)
   Python   ──▶  │  py-store (pip, host)        │ ─┘
                 └──────────────────────────────┘
```

- The **core** owns all pure logic.
- The **hosts** own driver IO, callbacks (sync computed columns, async computed columns) and placeholder substitution.
- The **bindings** are thin JSON bridges.

If you only want to *use* the data layer, install `nodejs-store` or `storepy` — you do not need this repo directly.

## Workspace layout

| Directory | Crate / package | Purpose |
| --- | --- | --- |
| `core/` | `rust-store-core` | Language-agnostic core: GQL / permissions / computed columns / command planning. Pure logic, no IO. |
| `core-node/` | `rust-store-node` | Node binding (napi-rs) → `dist/rust-store-node.node`. Published to npm as `rust-store-node`. |
| `core-py/` | `rust-store-py` | Python binding (PyO3) → `dist/rust_store_py.pyd`. Published to PyPI as `rust-store-py`. |

The workspace shares a single root `target/` and root `Cargo.lock`. All three crates are `publish = false` (nothing is published to crates.io; the bindings ship through npm and PyPI).

Internal modules worth knowing: `pipeline/` (GQL parse → AST → `$lookup`/`$group` build), `command/` (query/count/write/mutation planners), `dialect/` (filter, select, write, row rehydration, introspection, overlay), `computes/` (sync / async / agg), `permission.rs`, `federation/`, `schema/`.

## Installation

Bindings (what hosts depend on):

```bash
npm i rust-store-node          # Node binding; platform natives ship as optionalDependencies
pip install rust-store-py      # Python binding (maturin wheel)
```

Rust library:

```bash
cargo add rust-store-core     # pure logic: GQL → command JSON → SQL, no IO
```

API docs are built automatically on [docs.rs](https://docs.rs/rust-store-core). Working from a checkout instead? Use a path dependency: `rust-store-core = { path = "path/to/rust-store/core" }`.

Build toolchain for development: Rust stable (edition 2021) with `cargo` / `clippy` / `rustfmt`; `napi-rs` CLI for `core-node`; `maturin >= 1.7, < 2.0` for `core-py`.

Building the bindings locally:

```bash
# Node binding (core-node/)
npx napi build --platform --release          # release flow: .github/workflows/release-npm.yml

# Python binding (core-py/)
maturin build --manifest-path core-py/Cargo.toml --release
```

## Quick start

### Node.js (`core-node`, camelCase)

```js
const { Registry } = require('rust-store-node');

const reg = new Registry();
reg.register({ name: 'User', collection: 'users', fields: { name: { type: 'string' } }, relations: {} });

const plan = reg.planQuery('User{name}', {}, null);   // → MongoDB command JSON
```

Sync computed-column callbacks are registered with `setFn`; `asyncFn` computed columns use the two-phase `prepareQuery` / `stripQuery` flow.

### Python (`core-py`, snake_case)

```python
from rust_store_py import Registry

reg = Registry()
reg.register({"name": "User", "collection": "users", "fields": {"name": {"type": "string"}}, "relations": {}})

plan = reg.plan_query("User{name}", {}, None)  # → MongoDB command JSON (dict)
```

Sync computed columns use `set_fn`; `asyncFn` uses the same two-phase flow. **Errors are always raised as Python exceptions (`PyErr`)** — they are never mixed into the returned dict.

### System context

`ctx` is an explicit parameter on every plan method. `{ internal: true }` marks a *system call* (permission engine passes everything, no owner injection), which is semantically different from `undefined`/`None` (no context).

- Node: `const { Registry, systemContext } = require('rust-store-node')` → `systemContext()` returns `{ internal: true }`.
- Python: `rust_store_py.system_context()` (module-level function).

## API reference

Node and Python names correspond one-to-one (camelCase ↔ snake_case). All methods below exist in both bindings.

### Lifecycle and schema

| Node | Python | Notes |
| --- | --- | --- |
| `new Registry()` | `Registry()` | Explicit registry instance (no module-level global) |
| `register(defn)` | `register(defn)` | Registers a schema; auto-derives the `<Name>Deleted` archive table |
| `has(name)` / `list()` | `has(name)` / `list()` | |
| `setFn(fnRef, cb)` / `clearFns()` | `set_fn(fn_ref, cb)` / `clear_fns()` | Sync computed-column callbacks |
| `setRequireContext(bool)` / `requireContext()` | `set_require_context(bool)` / `require_context()` | Fail-secure switch (default off) |

> The binding `Registry` has **no** `get(name)` — hosts keep their own schema dictionary.

### Read path

`buildPipeline` / `build_pipeline`, `planQuery` / `plan_query`, `planQueryOne` / `plan_query_one`, `planQueryWithCount` / `plan_query_with_count`, `resolvePage` / `resolve_page`, `restoreSortOrder` / `restore_sort_order`, `planExists` / `plan_exists`, `planCount` / `plan_count`, `sortsByRelation` / `sorts_by_relation`.

Notes: `planQueryOne` forces `$limit(1)` when no explicit `$limit` is present. `planQueryWithCount`'s `total` is supplied by the host after it runs the `countCommand`.

### Write path

`planInsert` / `plan_insert`, `planInsertMany` / `plan_insert_many`, `planUpdate` / `plan_update`, `planUpdateMany` / `plan_update_many`, `planRemove` / `plan_remove`, `planArchiveDocs` / `plan_archive_docs`, `planUpsert` / `plan_upsert`, `planMutation` / `plan_mutation`, `applyWriteDefaults` / `apply_write_defaults`.

Notes:

- `planUpdate` / `planRemove` return `{ "needsProbe": cmd }` when a `creator` permission check requires a probe; the host executes the probe and re-enters with `probeFound` / `probeDoc` to get `{ "command": cmd }`.
- `planUpdateMany` rejects guest / unauthorised callers outright and **does not** use the creator probe.
- `planRemove` returns an archive `findCommand` plus a `deleteCommand`; archived documents are written to `<collection>_deleted` with a `deletedAt` field.
- `planMutation` expands into an ordered step sequence; parent/child dependencies are expressed with `{{step.<N>._id}}` placeholders that the host fills in.
- Every plan method takes an optional trailing `routeOverride` / `route_override` (`{source, namespace}`).

### Permissions

`canRead` / `can_read`, `canWrite` / `can_write`, `shouldInjectOwner` / `should_inject_owner`, `mergeOwnerCondition` / `merge_owner_condition`, `readableFields` / `readable_fields`, `readableRelations` / `readable_relations`, `writableFields` / `writable_fields`, `filterWritableData` / `filter_writable_data`.

### Computed columns and post-processing

`processNode` / `process_node`, `asyncFnRefs` / `async_fn_refs`, `injectDepends` / `inject_depends`, `stripDepInjected` / `strip_dep_injected`, `prepareQuery` / `prepare_query` (phase one, returns `{items, fnRefs}`), `stripQuery` / `strip_query` (phase three).

### Datasource, dialect, federation

| Node | Python | Notes |
| --- | --- | --- |
| `resolveDatasource(schemaName, config)` | `resolve_datasource(...)` | Returns `"mongo"` / `"mysql"` / `"postgres"` / `"sqlite"` |
| `schemaDatasource(schemaName)` | `schema_datasource(...)` | `null` when undeclared (semantics: `default`) |
| `dialectTranslate(backend, cmd)` | `dialect_translate(...)` | MongoDB command JSON → SQL statement sequence |
| `restoreRows(shape, rows)` | `restore_rows(...)` | Flat rows → nested documents |
| `schemaFromRows(rows, backend)` | `schema_from_rows(...)` | Introspection rows → schemaJSON |
| `mergeSchema(base, overlay)` | `merge_schema(...)` | Physical structure + local overlay |
| `planFederated(gql, params, ctx, dsConfig)` | `plan_federated(...)` | Splits one GQL into per-source commands + in-memory join edges; result includes `degraded` |
| `mergeFederated(plan, results)` | `merge_federated(...)` | `results` must match `plan.sources` in order and length |

Module-level: `systemContext()` (Node) / `system_context()` (Python).

**APIs that do not exist** (do not invent them): there is no `aggregate` passthrough, no public `parseGql` / `parse_gql` method (parsing is internal via `pipeline::parse_gql`), and **no IO / driver / execution method of any kind**.

## GQL capabilities

```text
ModelName($condition:@c0,$sort:@s1,$skip:@sk,$limit:@l1) {
  field1, field2, obj.subField,
  RelationName($condition:@c2,$sort:@s3,$limit:@l2) { field3, NestedRelation { field4 } }
}
```

- Values are referenced from the params object by `@key`.
- Relations are declared in the schema (`type: "many" | "one"`) and compiled by the engine into `$lookup` / `$addFields` — **never hand-write `$lookup`**.
- Recursion guards: `MAX_DEPTH = 10`, `MAX_PAGINATED_DEPTH = 4`.
- **`$pipeline` passthrough has been removed** — its presence is an explicit parse error, not a silent no-op.

### Root-level `$group` / `$having`

```text
Course($condition:@c0, $group:@g0, $having:@h0, $sort:@s0, $skip:@sk, $limit:@l0) { status, n, total }
```

- Spec: `{ "by": ["status","meta.level"], "agg": { "n": {"$count":"*"}, "total": {"$sum":"price"} } }`.
- Operator whitelist (identical on the `$group` and SQL-translation sides): `$count` / `$sum` / `$avg` / `$min` / `$max`; `$count: "*"` means row count.
- Fixed execution order: `$condition` (WHERE) → `$group` (GROUP BY) → `$having` (HAVING) → `$sort` → `$skip`/`$limit` → projection. With `$group`, sort/pagination apply to the **grouped result**, and the `$sort` key domain is `by` keys ∪ `agg` aliases.
- `$having` without `$group` is an error.
- Validation: `by` accepts scalar fields (including object dot-paths); relations / arrays / bare objects / out-of-schema fields are `Err`. `agg` accepts only scalar fields of the same table (relations / arrays / objects / dot-paths / foreign fields are `Err`).
- SQL translation (`dialect/select/group_agg.rs`): Mongo `$group` (`_id` + accumulators) → `GROUP BY` + aggregate columns; `$match` after `$group` → `HAVING`; whole-table single group (`by` omitted / `[]`) → no `GROUP BY`.

### Relation aggregate predicates (§9.6)

Filter parents by an aggregate over a relation — a semi-join with no fan-out:

- Shorthand: `{ "<relation>": { "$exists": true|false } }`, `{ "$count": { "$of"?: field, "<cmp>": value } }`, `{ "$sum"|"$avg"|"$min"|"$max": { "$of": field, "<cmp>": value } }`, optionally combined with a `$filter` block.
- Main form: `{ filter?, agg, having }`.
- Comparison operators: `$gt` / `$gte` / `$lt` / `$lte` / `$eq` / `$ne`.
- `$not` wrapping a single relation predicate, or `$exists: false`, produces an anti-join.
- SQL: `EXISTS` / `NOT EXISTS`; MongoDB: sentinel surrogate keys.
- One relation level only; `orders.items.price` is `Err`. A relation the caller cannot read is `Err`, never silently `false`. Cannot be combined with root-level `$group`.

### Computed columns

Declared in the schema; three forms:

| Form | Evaluated | Notes |
| --- | --- | --- |
| `fn` | host (sync, via `set_fn`) | dependencies are injected into the projection automatically |
| `asyncFn` | host (async, two-phase) | same injection, resolved after the query returns |
| `agg` | **engine inline** | `{"$count": "<relation>"}` or `{"$sum"|"$avg"|"$min"|"$max": "<relation>.<field>"}` |

`agg` is mutually exclusive with `fn` / `asyncFn`. Empty-set semantics: `$count → 0`, others → `None` (nullable). SQL uses a derived table `LEFT JOIN (… GROUP BY fk)`; MongoDB uses `$lookup` + `$addFields`.

## Backends and dialects

- **MongoDB** — native aggregation pipeline.
- **MySQL** — parameterized SQL, `information_schema` introspection.
- **SQLite** — parameterized SQL (`?`), `sqlite_master` + `PRAGMA` introspection.
- **PostgreSQL** — parameterized SQL (`$n`), `RETURNING` for read-after-write.

Datasource registration: the host passes `dsConfig = { "sources": { "<name>": "<kind>" } }` (`null` = single-source Mongo). SQL joins across namespaces of the same source are still pushed down (qualified `JOIN`); Mongo cross-database relations degrade to in-memory federation.

Cross-backend translations: root `$group` / `$having` → `GROUP BY` / `HAVING`; the `$count`/`$sum`/`$avg`/`$min`/`$max` whitelist; relation aggregate predicates → `EXISTS` / `NOT EXISTS` (`WHERE EXISTS (SELECT 1 … GROUP BY fk HAVING …)`); relation-rolling `agg` computed columns → derived table `LEFT JOIN (… GROUP BY fk)`; per-parent top-N (`$sort`/`$skip`/`$limit` inside a relation) → `ROW_NUMBER() OVER (PARTITION BY fk ORDER BY …)`.

**Mongo-only or explicitly-unsupported on SQL**: object dot-paths in `$group.by` (Mongo can execute, SQL cannot map to a scalar column → `Err`); root `$sort` keys that cannot be mapped (unknown field / object or array field / relation name itself / no matching relation drill-down) are simply not pushed down (warning + host-side fallback sort) while Mongo executes them normally.

## Permission model

Schema-level `read` / `write`, field-level `field.read` / `field.write`, relation-level `rel.read`, computed-column-level `comp.read`.

- `super_admin` / `admin` / `internal` pass everything.
- `guest` has no write permission regardless of schema configuration.
- `write: []` (empty whitelist) denies all writes.
- `creator` is a pseudo-role resolved dynamically as `doc.createdBy == ctx.userId`.
- The permission context is an **explicit parameter** (`ctx`) — this is a deliberate difference from older implicit `AsyncLocalStorage`-style designs.

Guarding helpers for AI query hosts: `timestamps` value validation (only `true` / `false` / `"ms"` / `"s"`, invalid values fail at registration) and federation `degraded` events (`{code, layer, message, hint}`, returned in `plan.degraded`) so non-pushdownable cross-source pagination/sort never blocks a query silently. See `core/tests/guards.rs`.

## Testing and parity

```bash
# 1) core (pure Rust, no host dependency)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p rust-store-core

# 2) Node binding (core-node/)
npm ci
npx napi build --platform
npm test

# 3) Python binding (repo root)
cd core-py && pip install maturin pytest && maturin build --out dist && pip install --force-reinstall dist/*.whl
cd .. && python -m pytest core-py/test/parity_test.py -v

# 4) Golden baseline, recomputed on all three sides
node tools/verify-fixtures.js
```

- Core tests: `core/tests/parity*.rs` (parity, parity_computes, parity_commands, parity_write, parity_fnfns, parity_dialect, parity_federation), plus `guards.rs`, `pushdown_usecases.rs`, `regression_d_fixes.rs`, `route_override.rs`.
- Binding parity: `core-node/test/{parity.test.js, dialect.smoke.test.js, t2q.skill.test.js}` and `core-py/test/parity_test.py`.
- Golden fixtures: `fixtures/{pipeline,commands,computes,fnfns,federation,expected,host}/`. There is **no generator** — `tools/verify-fixtures.js` recomputes every case through all three implementations and deep-compares against the frozen snapshot; all three green means "no diff, reproducible".
- **The binding crates have no Rust unit tests** (`[lib] test = false`), so `cargo test --workspace` does not cover them — always run the host-side parity suites after changing a binding.

## Boundaries and gotchas

1. **No clock, no randomness in `core`.** `now` / `newId(s)` must be supplied by the host, which is what makes cross-language results reproducible.
2. **`ctx` is explicit** on every plan method. Omitting it means "no context" (permissive by default; an error when `require_context` is on).
3. **`require_context` defaults to off (fail-open)** for parity with the original JS implementation. Turn it on at host startup and pass `systemContext()` for internal calls to get fail-secure behaviour.
4. **Stable error prefixes**: `ERR_PERM_PREFIX`, `ERR_NO_WRITE`, `ERR_NO_BATCH_WRITE`, `ERR_NO_CONTEXT` (`core/src/command/mod.rs`). Hosts map these to their own error types. Python must raise them as `PyErr`, never return them.
5. **Empty-condition batch writes are rejected**: `updateMany` / `remove` with `{}`, `null` or an empty logical group (`{"$and":[]}` / `{"$or":[]}`) is treated as unconditional and explicitly refused — it never touches a whole table.
6. **`__present` is an internal SQL sentinel column**: it distinguishes "explicit null (key present)" from "missing (no key)". It is injected and consumed by the translation layer; in PostgreSQL `ON CONFLICT DO UPDATE`, references must be table-qualified or you get `column reference "__present" is ambiguous`.
7. **U1–U4 are global errors**: filtering directly on array fields (U1), deep equality on object fields (U2), object dot-path filtering (U3) and object dot-path sorting (U4) all raise on every backend; empty logical groups raise too. Relation-path sorting is *not* object dot-path sorting and is unaffected.
8. **`timestamps` validation**: only `true` / `false` / `"ms"` / `"s"` (default ms); invalid values fail at registration. Unit conversion is the host's clock's job.
9. **Federation `degraded` does not block**: non-pushdownable cross-source pagination/sort produces structured events the host is expected to feed into an automated feedback loop.
10. **Untranslatable means explicit** (project rule): translation must raise or emit `unsupported` + warning; it never emits SQL that is missing a clause.
11. **`planFederated` results must match `plan.sources` in order and length**, or `mergeFederated` will misalign.

## FAQ

**Is `rust-store` an ORM?**
No. It is a planning and translation engine. It holds no driver, opens no connection and executes nothing — the host runs every command.

**Do I need this repo to use the data layer?**
No. Install `nodejs-store` (npm) or `storepy` (PyPI). This repo matters if you are building, debugging or extending the engine itself, or writing a host for another language.

**How do I write GQL queries?**
See [GQL capabilities](#gql-capabilities). The full syntax and examples are also documented in the companion `text-to-query` skill, which turns natural-language questions into GQL + params.

**Why do Node.js and Python behave identically?**
Both bindings wrap the same Rust core and only convert JSON. No logic is duplicated in the hosts, so semantics cannot drift. The parity suites and golden fixtures exist to prove this continuously.

**How does aggregation work across four different databases?**
Root-level `$group` / `$having` map to `GROUP BY` / `HAVING`; relation aggregate predicates map to `EXISTS` / `NOT EXISTS`; relation-rolling `agg` computed columns map to a derived-table `LEFT JOIN`. MongoDB uses its native pipeline. All four backends are covered by the same semantics.

**What happens when something cannot be translated to SQL?**
The engine raises explicitly or emits an `unsupported` + warning event. It never produces SQL that silently omits a clause. MongoDB can still execute a few things SQL cannot (e.g. object dot-paths in `$group.by`), which is why those cases are errors only on the SQL side.

**Can I use it from Rust directly?**
Yes — `rust-store-core` is a plain Rust library (`publish = false`, so depend on it by path). You register schemas, plan queries and translate commands, then execute them with the driver of your choice.

## Related projects

- [`nodejs-store`](https://github.com/coenddt/nodejs-store) — Node.js host (npm `nodejs-store`).
- [`py-store`](https://github.com/coenddt/py-store) — Python host (pip `storepy`).
- `text-to-query` — a companion skill that compiles natural-language questions into GQL + params for this engine.

## License

[MIT](LICENSE)
