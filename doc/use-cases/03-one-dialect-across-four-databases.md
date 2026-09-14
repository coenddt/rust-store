# One dialect across four databases

## The problem

Your data does not live in one kind of database. Some collections are in MongoDB, some
tables are in MySQL, some in PostgreSQL, and a local cache or edge deployment uses SQLite.
You do not want four query languages, four pagination conventions and four aggregation
dialects — you want to write a query once and have it mean the same thing everywhere.

## Why rust-store

The engine fixes MongoDB's dialect as the canonical one. A query is planned into **MongoDB
command JSON** first, and a pure translation layer turns that same command into
parameterized SQL for MySQL, PostgreSQL and SQLite. Drivers return flat JOIN rows, which
the engine rehydrates into the nested documents your application expects. The same semantics
apply on all four backends.

The public translation entry point is `dialectTranslate` / `dialect_translate` (Rust:
`rust_store_core::dialect::translate`), and rehydration is `restoreRows` / `restore_rows`.

## Walkthrough

Plan a query once, then translate the resulting command to each relational backend:

```js
const { Registry } = require('rust-store-node');

const reg = new Registry();
reg.register({
  name: 'Course',
  collection: 'courses',
  fields: { status: { type: 'string' }, price: { type: 'float' } },
  relations: {},
});

// 1) Root-level `$group` / `$having` plans to a single MongoDB command.
const gql = 'Course($condition:@c0,$group:@g0,$having:@h0,$sort:@s0,$limit:@l0){ status, n, total }';
const params = {
  c0: { status: { $ne: 'deleted' } },
  g0: { by: ['status'], agg: { n: { $count: '*' }, total: { $sum: 'price' } } },
  h0: { n: { $gt: 1 } },
  s0: { total: -1 },
  l0: 20,
};

const plan = reg.planQuery(gql, params, null);
const cmd = plan.commands[0]; // aggregate command JSON (MongoDB dialect)

// 2) The same command becomes parameterized SQL for three relational backends.
for (const backend of ['mysql', 'postgres', 'sqlite']) {
  const out = reg.dialectTranslate(backend, cmd);
  // out = { backend, stmts, warnings, unsupported }
  console.log(backend, out.stmts[0].text); // GROUP BY + HAVING (+ ORDER BY / LIMIT)
  if (out.unsupported.length > 0) {
    // A segment could not be pushed down safely. `stmts` does NOT contain it:
    // reject or degrade — never treat the SQL as complete.
  }
}
```

Translate a relation aggregate predicate, and rehydrate the rows a driver returns:

```js
// (same `reg` as above)
// 3) A semi-join predicate over a relation becomes EXISTS / NOT EXISTS.
reg.register({
  name: 'Product',
  collection: 'products',
  fields: { name: { type: 'string' }, status: { type: 'string' } },
  relations: {
    orders: { model: 'Order', type: 'many', localField: '_id', foreignField: 'productId' },
  },
});
reg.register({
  name: 'Order',
  collection: 'orders',
  fields: { productId: { type: 'string' } },
  relations: {},
});

const semi = reg.planQuery(
  'Product($condition:@c0){ _id, name }',
  { c0: { $and: [ { status: 'onSale' }, { orders: { $count: { $gt: 3 } } } ] } },
  null,
);
const pg = reg.dialectTranslate('postgres', semi.commands[0]);
console.log(pg.stmts[0].text); // ... WHERE EXISTS (SELECT 1 ... GROUP BY fk HAVING ...)

// 4) The driver returns flat JOIN rows; `rowShape` (emitted with the statement) drives
//    rehydration back into nested documents.
const { rowShape } = pg.stmts[0];
const rows = /* flat rows returned by your driver */ [];
const docs = reg.restoreRows(rowShape, rows);
```

Cross-backend translations the engine performs:

| GQL / plan feature | MongoDB | MySQL / PostgreSQL / SQLite |
| --- | --- | --- |
| root `$group` / `$having` | `$group` / `$match` | `GROUP BY` / `HAVING` |
| `$count` / `$sum` / `$avg` / `$min` / `$max` | native accumulators | aggregate columns (whitelist) |
| relation aggregate predicate | `$lookup` + sentinel surrogate keys | `EXISTS` / `NOT EXISTS` |
| relation-rolling `agg` computed column | `$lookup` + `$addFields` | derived table `LEFT JOIN (… GROUP BY fk)` |
| per-parent top-N (relation `$sort`/`$skip`/`$limit`) | native pipeline | `ROW_NUMBER() OVER (PARTITION BY fk ORDER BY …)` |

## Pitfalls

- **`unsupported` means incomplete.** `dialectTranslate` returns
  `{ backend, stmts, warnings, unsupported }`. When `unsupported` is non-empty, `stmts`
  deliberately omits that segment; the host must reject or degrade, never run it as if it
  were the whole query.
- **Mongo-only cases.** Object dot-paths in `$group.by` are executable on MongoDB but cannot
  map to a scalar column, so SQL translation raises `Err`. Root `$sort` keys that cannot be
  mapped (unknown field / object or array field / relation name itself / no matching
  relation drill-down) are not pushed down (warning + host-side fallback sort), while
  MongoDB executes them normally.
- **Placeholders and quoting differ.** PostgreSQL uses `$n` placeholders; MySQL and SQLite
  use `?`. Identifiers are back-quoted on MySQL and double-quoted elsewhere — never build
  SQL by hand, always go through `dialectTranslate`.
- **`restoreRows` needs the matching `rowShape`.** Pass the shape emitted with the exact
  statement you ran, not a shape from a different backend or query.
- **MySQL has no `RETURNING`.** Read-after-write is native on PostgreSQL and SQLite, but on
  MySQL the host orchestrates `UPDATE` + `find` itself.
- **`$pipeline` is gone.** There is no aggregate-pipeline escape hatch: a `$pipeline` in GQL
  is an explicit parse error.

## See also

- [README — Backends and dialects](../../README.md#backends-and-dialects)
- [README — GQL capabilities](../../README.md#gql-capabilities)
- [Embed the core in a Rust service](01-embed-the-core-in-a-rust-service.md)
- [Node.js / Python parity](04-node-python-parity.md)
