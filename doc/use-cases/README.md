# Use cases

Scenario walkthroughs for `rust-store`.

Each document starts from a concrete problem, explains why the engine fits, then walks
through a small example built **only from documented APIs**. Read them alongside the
[main README](../../README.md).

| # | Scenario | In one line |
| --- | --- | --- |
| [01](01-embed-the-core-in-a-rust-service.md) | Embed the core in a Rust service | Use `rust-store-core` as a path dependency: register schemas, plan a query, translate it to SQL, and hand the command to your own driver. |
| [02](02-natural-language-to-query-compiler.md) | Natural language → query compiler | Turn model output into a reviewable plan: compile GQL to a plan *before* anything touches a database, and classify rejections by stable error prefixes. |
| [03](03-one-dialect-across-four-databases.md) | One dialect across four databases | Write GQL once, plan to MongoDB command JSON, then translate the same command to parameterized MySQL / PostgreSQL / SQLite. |
| [04](04-node-python-parity.md) | Node.js / Python parity | One Rust core, two bindings: camelCase vs snake_case is the only difference, and parity suites plus golden fixtures keep it that way. |

## See also

- [README](../../README.md) — architecture, API reference, GQL capabilities, dialects, permission model, parity testing, gotchas.
- [rust-store skill](../../.trae/skills/rust-store/SKILL.md) — condensed repo guide for agents.
