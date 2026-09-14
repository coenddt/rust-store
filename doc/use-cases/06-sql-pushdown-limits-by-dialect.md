# SQL pushdown limits by dialect

## The problem

You are choosing a backend — or mixing several — and you need to know, before
committing, what each one can execute natively and what the engine will refuse, degrade
or hand back to your host. "It translates MongoDB commands to SQL" is not enough: the
interesting part is the boundary.

## Why rust-store

The engine fixes MongoDB's dialect as canonical. A query is planned into MongoDB command
JSON, and a pure function translates that command into parameterized SQL for MySQL,
PostgreSQL and SQLite. The public entry point is `dialectTranslate` / `dialect_translate`
(Rust: `rust_store_core::dialect::translate`), and it returns
`{ backend, stmts, warnings, unsupported }`. It never emits SQL that is quietly missing a
clause: a segment it cannot translate safely leaves `stmts` and is surfaced as an explicit
error or an `unsupported` entry.

## Walkthrough

```js
const { Registry } = require('rust-store-node');

const reg = new Registry();
reg.register({
  name: 'Course',
  collection: 'courses',
  fields: { status: { type: 'string' }, price: { type: 'float' } },
  relations: {},
});

const plan = reg.planQuery('Course($condition:@c0,$sort:@s0){ status, price }', {
  c0: { price: { $gte: 10 } },
  s0: { price: -1 },
}, null);
const cmd = plan.commands[0]; // MongoDB command JSON (the canonical dialect)

for (const backend of ['mysql', 'postgres', 'sqlite']) {
  try {
    const out = reg.dialectTranslate(backend, cmd);
    // out.stmts       : statements that WERE pushed down
    // out.warnings    : notes (e.g. a $regex flag the backend cannot express)
    // out.unsupported : [{ code, field, reason }] — segments deliberately NOT in stmts
    for (const u of out.unsupported) {
      console.warn('not pushed down:', u.code, u.field); // host does a fallback sort
    }
  } catch (err) {
    // Hard failure: the command (or a stage) cannot be translated at all.
    // Reject it — never run partial SQL as if it were the whole query.
    console.error('translation refused:', String(err));
  }
}
```

Cross-source work is reported the same way, as structured events rather than a silent
wrong answer:

```js
const fed = reg.planFederated(
  'User{ _id, name, orders($sort:@s0){ code } }',
  { s0: { code: 1 } },
  null,
  { sources: { default: 'mongo', analytics: 'postgres' } },
);

for (const ev of fed.degraded) {
  // ev = { code, layer, message, hint }, layer = "federation"
  // code: 'crossSourceChildPaging' | 'crossSourceSort'
  // `degraded` does not block: the host sorts / paginates in memory after mergeFederated.
  console.warn(ev.code, ev.hint);
}
```

What each backend can do:

| Feature | MongoDB | MySQL | PostgreSQL | SQLite |
| --- | --- | --- | --- | --- |
| Plan target | native aggregation pipeline | parameterized SQL (`?`) | parameterized SQL (`$n`) | parameterized SQL (`?`) |
| Identifier / table naming | database in the location triple | back-quoted, qualified `` `ns`.`t` `` | double-quoted `"ns"."t"` | double-quoted `"ns"."t"` |
| Offset without a limit | native | `LIMIT 18446744073709551615 OFFSET ?` | `OFFSET $n` | `LIMIT -1 OFFSET ?` |
| Read-after-write | native | none — host runs `UPDATE` + `find` | `RETURNING` | `RETURNING` |
| Non-integer literal binding | native | dynamic typing | `CAST($n AS double precision)` | dynamic typing |
| `AVG` | IEEE-754 double | `AVG(CAST(x AS DOUBLE))` | `AVG(CAST(x AS DOUBLE PRECISION))` | `AVG(CAST(x AS REAL))` |
| `$regex` with `$options: "i"` | native flags | `REGEXP_LIKE(col, ?, 'i')` (MySQL 8.0+) | `col ~* $n` | flags unsupported: warning, semantics degrade to case-sensitive |
| Root `$group` / `$having` | `$group` / `$match` | `GROUP BY` / `HAVING` | `GROUP BY` / `HAVING` | `GROUP BY` / `HAVING` |
| `$group.by` object dot-path | executes | error | error | error |
| Relation-rolling `agg` computed column | `$lookup` + `$addFields` | derived table `LEFT JOIN (… GROUP BY fk)` | same | same |
| Relation aggregate predicate | `$lookup` + sentinel surrogate keys | `EXISTS` / `NOT EXISTS` | same | same |
| Per-parent top-N (relation `$skip` / `$limit`) | native pipeline | `ROW_NUMBER() OVER (PARTITION BY fk ORDER BY …)` | same | same |
| Relation crossing namespaces of one source | stripped → in-memory federation | pushed down as a qualified `JOIN` | same | same |
| Root `$sort` key that cannot be mapped | executes natively | not pushed down (`unsupported`) | same | same |

Operators and clauses that push down as SQL: `$eq` / `$ne` (null-aware — `$eq: null`
becomes `IS NULL` plus the `__present` existence check), `$gt` / `$gte` / `$lt` / `$lte`,
`$in` / `$nin`, `$exists` (via the `__present` sentinel), `$not`, and `$regex`. Operators
with no translation — `$expr`, `$elemMatch`, `$all`, `$where` and any other — are an
explicit error, never a dropped condition. The U1–U4 shapes (array-field filter, object
deep-equality, object dot-path filter, object dot-path sort) raise on every backend.

When pushdown is impossible there are exactly two outcomes:

- **Soft — `unsupported`.** `warnings` and an `unsupported` entry are appended, and
  `stmts` deliberately omits that segment; the host must reject or degrade. The only
  `unsupported` code emitted today is `{ "code": "sortField", "field": …,
  "reason": … }` for a root `$sort` key with no scalar column, whose fallback is a
  host-side sort.
- **Hard — an error.** In the Rust core these are `Result<_, String>`; at the Node /
  Python binding boundary they are classified into `CoreError`
  (`CoreError::Other` for a translation or validation failure, `CoreError::Permission`,
  `CoreError::NoContext`) and raised as a native error. Hard cases include an
  unimplemented aggregation stage, an explicit projection of an `object` / `array` field,
  a `$unwind` path that is not a `one`-relation `$lookup` product, an `$addFields` that
  does not resolve to `agg` computed columns, and a `$group` accumulator outside the
  `$count` / `$sum` / `$avg` / `$min` / `$max` whitelist.

The feedback event that reports degraded work is `degraded` — `planFederated` returns it
as `plan.degraded`, a list of `{ code, layer, message, hint }`. The codes the federation
planner emits are `crossSourceChildPaging` (a relation `$sort` / `$skip` / `$limit` that
cannot be pushed per parent across sources) and `crossSourceSort` (a root `$sort` on a
cross-source relation field), both with `layer: "federation"`.

MongoDB-specific `$lookup` and aggregation differences:

- Relations are compiled by the engine into `$lookup` / `$addFields`; a hand-written
  `$lookup` is not the contract, and a `$pipeline` passthrough is an explicit parse error.
- `$lookup` cannot cross databases. Same source *and* same namespace pushes down; same
  source but a different database is stripped and planned as in-memory federation, where
  a SQL backend would have pushed the same case down as a qualified `JOIN`.
- The two-phase optimisation applies only to MongoDB: a `$lookup` followed by
  `$skip` / `$limit` becomes two aggregate commands, and the second carries the
  `PHASE1_IDS` placeholder (`{{phase1.ids}}`) that the host replaces before executing,
  with `restore_sort_order` re-applying the order afterwards. On SQL the same query is a
  single statement.
- Relation aggregate predicates stay in phase one — their `$lookup` uses `REL_PRED_PREFIX`
  — so predicate filtering is not pushed into phase two and does not distort pagination.
- The empty-set guard for a whole-table group is MongoDB-only: MongoDB appends
  `$facet` + `$replaceRoot` so the group returns one row over empty input. On SQL those
  stages are a no-op (a query without `GROUP BY` already returns one row) and are ignored
  only inside the group path.

## Pitfalls

- **`unsupported` means incomplete.** When `unsupported` is non-empty, `stmts` omits that
  segment on purpose; reject or degrade, never execute it as the whole query.
- **Mongo-only cases are not "bugs to fix".** `$group.by` object dot-paths execute on
  MongoDB but cannot map to a scalar column, so SQL translation errors; an unmappable root
  `$sort` key is skipped with a warning while MongoDB executes it normally.
- **Placeholders and quoting differ.** PostgreSQL uses `$n`; MySQL and SQLite use `?`.
  Identifiers are back-quoted on MySQL and double-quoted elsewhere — never build SQL by
  hand, always go through `dialectTranslate`.
- **SQLite needs a host-registered `REGEXP`.** SQLite does not ship the function;
  register it on the connection (`regexp(pattern, value)`) or `REGEXP` fails at execution.
  `$options` other than `i` cannot be expressed on any backend.
- **MySQL has no `RETURNING`.** `Backend::supports_returning()` is false only for MySQL;
  there the host must orchestrate the read-after-write itself.
- **Hard errors are not warnings.** An unsupported aggregation stage or object/array
  projection is a raised error, not a degraded result — catch it and reject the query.
- **`degraded` does not block.** Cross-source pagination / sort events describe work the
  host must do after `mergeFederated`; treating them as "the query succeeded as written"
  silently returns the wrong rows.
- **`restoreRows` needs the matching `rowShape`.** Pass the shape emitted with the exact
  statement you ran, not a shape from another backend or query.

## See also

- [README — Backends and dialects](../../README.md#backends-and-dialects)
- [README — GQL capabilities](../../README.md#gql-capabilities)
- [README — Boundaries and gotchas](../../README.md#boundaries-and-gotchas)
- [One dialect across four databases](03-one-dialect-across-four-databases.md)
- [Reuse the permission model](05-reuse-the-permission-model.md)
