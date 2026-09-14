# Natural language → query compiler

## The problem

You are building an AI / natural-language query layer. A model turns a business question
into *something* — and that something must reach the database only after you have had a
chance to inspect, validate and possibly reject it. If the model's output is executed
directly, a hallucinated field, an over-broad filter or an unauthorised read becomes a
production incident.

## Why rust-store

Planning is a **pure function**: `gql → plan`. The core compiles GQL plus params into a
plan (MongoDB command JSON) without touching a database, so a plan can be inspected,
rejected or logged before a single command runs. Nothing in the engine executes anything.

Around that, two guardrails exist specifically for query hosts:

- a `text-to-query` workflow turns the natural-language question into **GQL + params**
  (the engine's input shape), never into raw SQL;
- stable error prefixes and `degraded` events let the host classify a rejection instead of
  pattern-matching on message text.

## Walkthrough

The `text-to-query` step produces a GQL string and a params object. The host then compiles
it with the binding — camelCase here for Node.js:

```js
const { Registry } = require('rust-store-node');

const reg = new Registry();
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
  fields: { productId: { type: 'string' }, amount: { type: 'float' } },
  relations: {},
});

// 1) Output of the text-to-query step: GQL + params (never raw SQL).
const gql = 'Product($condition:@c0,$sort:@s0){ _id, name }';
const params = {
  c0: { $and: [ { status: 'onSale' }, { orders: { $count: { $gt: 3 } } } ] },
  s0: { name: 1 },
};

// 2) Compile. This is a pure `gql -> plan` call: no database is touched.
//    `ctx` is the end user's context, so permission rules apply during planning.
const ctx = { userId: 'u_42' };

let plan;
try {
  plan = reg.planQuery(gql, params, ctx);
} catch (err) {
  // 3) Reject unsafe / unauthorised output explicitly, and classify by stable prefix.
  const msg = String(err);
  if (msg.startsWith('ERR_PERMISSION:')) {
    // permission denial -> 403 for the caller
  } else if (msg.startsWith('ERR_NO_CONTEXT')) {
    // contract violation (require_context on, ctx missing) -> 500, not 403
  } else {
    throw err; // strict parse / validation error: the model produced invalid GQL
  }
  return;
}

// The plan is plain JSON: log it, diff it, apply policy, and only then execute it.
console.log(JSON.stringify(plan.commands, null, 2));

// 4) Federated planning surfaces non-pushdownable work as structured events instead of
//    silently returning a wrong answer.
const fed = reg.planFederated(gql, params, ctx, {
  sources: { default: 'mongo', analytics: 'postgres' },
});
for (const ev of fed.degraded) {
  // ev = { code, layer, message, hint } — forward these into your feedback loop.
  console.warn(ev.code, ev.hint);
}
```

The error prefixes come from `core/src/command/mod.rs` and are stable contract, not prose:

| Prefix | Meaning | Typical host mapping |
| --- | --- | --- |
| `ERR_PERM_PREFIX` (`ERR_PERMISSION:`) | permission denial (schema / field / relation / computed column) | 403 |
| `ERR_NO_WRITE` | caller has no write permission | 403 |
| `ERR_NO_BATCH_WRITE` | caller has no batch-write permission | 403 |
| `ERR_NO_CONTEXT` | `require_context` is on and `ctx` is missing | 500 (contract violation) |

Because these are prefixes, the host matches on the prefix and strips it — the engine's
human-readable text is free to change.

## Pitfalls

- **Plan-only, always.** There is no execution / driver / IO method anywhere in the engine.
  You run `plan.commands` yourself; the plan cannot "accidentally" hit a database.
- **Do not trust model output — the parser will not either.** Unknown operators, a
  `$pipeline` passthrough, array-field filters (U1), object deep-equality (U2) and object
  dot-path filters / sorts (U3 / U4) all raise on every backend.
- **There is no public `parseGql`.** Parsing is internal (`pipeline::parse_gql`); parse and
  validation errors surface through the plan methods, so wrap the call and handle them.
- **`planFederated` output must be fed back verbatim.** `mergeFederated(plan, results)`
  requires `results` to match `plan.sources` **in order and length**; reordering breaks the
  join.
- **`degraded` does not block.** Cross-source pagination / sort that cannot be pushed down
  produces structured events — treat them as work for the host (fallback sort, reject, or a
  feedback loop), never as "the query succeeded as written".
- **Fail-open by default.** `require_context` defaults to off; turn it on for fail-secure
  planning, and pass `systemContext()` / `system_context()` only for genuinely internal
  calls (never to run a user query).
- **Python raises, it does not return errors.** In `rust-store-py`, failures are `PyErr`
  exceptions and are never mixed into the returned dict.

## See also

- [README — When to use it](../../README.md#when-to-use-it)
- [README — Permission model](../../README.md#permission-model)
- [README — Boundaries and gotchas](../../README.md#boundaries-and-gotchas)
- [One dialect across four databases](03-one-dialect-across-four-databases.md)
