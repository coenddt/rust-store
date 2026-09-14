---
title: "rust-store documentation"
description: "Documentation for rust-store — the Rust core engine behind nodejs-store and py-store: GQL parsing, permission checks, computed columns, command planning and SQL dialect translation."
---

# rust-store

**A single Rust core engine for multi-backend data access — GQL parsing, permission checks, computed columns, command planning and SQL dialect translation, exposed to Node.js and Python through native bindings.**

`rust-store` is the engine behind [`nodejs-store`](https://github.com/coenddt/nodejs-store) and [`py-store`](https://github.com/coenddt/py-store). It contains **no database driver and performs no IO**: it turns a query (GQL) or a write into a backend-agnostic **command** (MongoDB command JSON) for its host to execute, and translates those commands into parameterized SQL for MySQL / PostgreSQL / SQLite.

```bash
cargo add rust-store-core     # Rust core
npm install rust-store-node   # Node.js binding (napi-rs)
pip install rust-store-py     # Python binding (PyO3)
```

## Scenario walkthroughs

Six end-to-end walkthroughs, each with runnable code, the mistakes people make, and the exact limits of the engine:

| Scenario | What it covers |
| --- | --- |
| [01 — Embed the core in a Rust service](use-cases/01-embed-the-core-in-a-rust-service.html) | Use `rust-store-core` as a path dependency: register schemas, plan a query, translate it to SQL, hand the command to your own driver. |
| [02 — Natural language → query compiler](use-cases/02-natural-language-to-query-compiler.html) | Turn model output into a reviewable plan before anything touches a database, and classify rejections by stable error prefixes. |
| [03 — One dialect across four databases](use-cases/03-one-dialect-across-four-databases.html) | Write GQL once, plan to MongoDB command JSON, then translate the same command to parameterized MySQL / PostgreSQL / SQLite. |
| [04 — Node.js / Python parity](use-cases/04-node-python-parity.html) | One Rust core, two bindings: camelCase vs snake_case is the only difference, kept honest by parity suites and golden fixtures. |
| [05 — Reuse the permission model](use-cases/05-reuse-the-permission-model.html) | Ownership and role checks live in the schema: the context shape, the `creator` pseudo-role, owner-condition injection, and the `routeOverride` caveat. |
| [06 — SQL pushdown limits by dialect](use-cases/06-sql-pushdown-limits-by-dialect.html) | What each backend pushes down natively, what degrades to `degraded` events, and what the engine refuses outright. |

## Documentation

- [Full README](readme.html) — architecture, API reference, GQL capabilities, dialects, permission model, parity testing, gotchas.
- [Use-cases index](use-cases/) — the walkthrough list above with summaries.

## Hosts and bindings

- [`nodejs-store`](https://github.com/coenddt/nodejs-store) — Node.js host (npm `nodejs-store`), binding `rust-store-node`.
- [`py-store`](https://github.com/coenddt/py-store) — Python asyncio host (pip `storepy`), binding `rust-store-py`.

## Links

- npm: [rust-store-node](https://www.npmjs.com/package/rust-store-node)
- PyPI: [rust-store-py](https://pypi.org/project/rust-store-py/)
- crates.io: [rust-store-core](https://crates.io/crates/rust-store-core)
- Source: [github.com/coenddt/rust-store](https://github.com/coenddt/rust-store)
- Machine-readable summary: [llms.txt](llms.txt)
