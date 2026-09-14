# Node.js / Python parity

## The problem

You ship the same product as a Node.js service and a Python service. If the two services
each carry their own query, permission and computed-column logic, they will drift — one
fixes a pagination bug, the other keeps it; one honours a field-level permission, the other
forgets. Users see two different products behind the same API.

## Why rust-store

There is exactly one implementation of the semantics: the Rust core. The Node binding
(`rust-store-node`, napi-rs) and the Python binding (`rust-store-py`, PyO3) are thin bridges
that convert JSON in and JSON out. They add **no behaviour**, so the two languages cannot
disagree about what a query means.

The only surface difference is naming: camelCase on the Node side, snake_case on the Python
side. Everything else — command JSON, error semantics, computed-column flow — is identical.

## Walkthrough

The same registry and the same query, on both sides:

```js
// Node.js: rust-store-node (camelCase)
const { Registry, systemContext } = require('rust-store-node');

const reg = new Registry();
reg.register({ name: 'User', collection: 'users', fields: { name: { type: 'string' } }, relations: {} });

const plan = reg.planQuery('User{name}', {}, null); // → MongoDB command JSON
console.log(JSON.stringify(plan));
```

```python
# Python: rust-store-py (snake_case)
from rust_store_py import Registry

reg = Registry()
reg.register({"name": "User", "collection": "users", "fields": {"name": {"type": "string"}}, "relations": {}})

plan = reg.plan_query("User{name}", {}, None)  # → MongoDB command JSON (dict)
print(plan)
```

Both print the same command JSON. The names line up one-to-one:

| Node (camelCase) | Python (snake_case) |
| --- | --- |
| `planQuery` | `plan_query` |
| `planQueryOne` / `planQueryWithCount` | `plan_query_one` / `plan_query_with_count` |
| `planFederated` / `mergeFederated` | `plan_federated` / `merge_federated` |
| `dialectTranslate` / `restoreRows` | `dialect_translate` / `restore_rows` |
| `setFn` / `clearFns` | `set_fn` / `clear_fns` |
| `systemContext()` (module-level) | `system_context()` (module-level) |

Parameters are positional and correspond across the two bindings (cross-language parity is
prioritised over parameter count). **One important semantic asymmetry:** Python raises every
error as a `PyErr` exception; it is never mixed into the returned dict.

The parity suites prove the equivalence continuously:

```bash
# 1) core: pure Rust, all semantics live here
cargo test -p rust-store-core        # core/tests/parity*.rs
#    parity, parity_computes, parity_commands, parity_write,
#    parity_fnfns, parity_dialect, parity_federation

# 2) Node binding
cd core-node && npm ci && npx napi build --platform && npm test
#    test/parity.test.js, test/dialect.smoke.test.js, test/t2q.skill.test.js

# 3) Python binding
cd core-py && pip install maturin pytest && maturin build --out dist && pip install --force-reinstall dist/*.whl
cd .. && python -m pytest core-py/test/parity_test.py -v

# 4) golden fixtures, recomputed on all three sides
node tools/verify-fixtures.js
```

Why semantics cannot drift:

- all pure logic (GQL parsing, permissions, computed columns, planning, dialect translation)
  lives in the Rust core and nowhere else;
- the bindings only convert and forward JSON, so there is no second implementation to fall
  out of sync;
- the golden fixtures under `fixtures/{pipeline,commands,computes,fnfns,federation,expected,host}/`
  are frozen snapshots that `tools/verify-fixtures.js` recomputes through **all three**
  implementations and deep-compares; all three green means "no diff, reproducible".

## Pitfalls

- **Do not mix naming conventions.** Calling `plan_query` on the Node binding or `planQuery`
  on the Python binding fails — the bridge is a literal name map, not a fuzzy one.
- **The binding crates have no Rust tests.** They are `[lib] test = false`, so
  `cargo test --workspace` does not cover them. After changing a binding, always run the
  host-side parity suites.
- **The golden fixtures have no generator.** Do not try to regenerate the baseline from the
  current core; `tools/verify-fixtures.js` only recomputes and compares against the frozen
  snapshot.
- **Errors differ in shape, not meaning.** Node throws; Python raises `PyErr`. Both must be
  handled as failures, and Python must never return an error inside the result dict.
- **`ctx` is the same explicit parameter on both sides.** `undefined` / `None` means "no
  context"; `require_context` defaults to off (fail-open) in both bindings.

## See also

- [README — Testing and parity](../../README.md#testing-and-parity)
- [README — FAQ](../../README.md#faq)
- [Embed the core in a Rust service](01-embed-the-core-in-a-rust-service.md)
- [One dialect across four databases](03-one-dialect-across-four-databases.md)
