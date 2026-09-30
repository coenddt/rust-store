# Transactional Capabilities in the Core (relation predicates / autoincrement / group-by paths)

> Engine-side notes for the transactional capability additions shared by both hosts
> (py-store / nodejs-store). Host-facing API and examples live in each host's
> `doc/transaction-capabilities.md`. Change history: common-store repo
> (`事务型能力增补执行文档.md`).

## 1. Relation predicates: new sugar + mutation normalization (`core/src`)

- **Plain condition object sugar** (`pipeline/relation_filter.rs::parse_simple`): a predicate
  spec whose keys carry no `$` prefix and no main-form keys (`filter`/`agg`/`having`) is now
  treated as `$filter: {…}` + `$exists: true` (semi-join). Specs containing any `$` key keep
  the original parsing (existing J-group negative-case messages are byte-identical).
- **Mutation normalization** (`command/mutate/mod.rs::plan_rel_pred_mutation`): when an
  `updateMany` / `remove` condition references a relation name, planning emits
  `plan.lookups + {$match: <rewritten condition>} + {$project: {_id: 1}}` as a **preCommand**
  and rewrites the main command's condition to `_id $in` with a host-filled placeholder
  (`__REL_PRED_IDS__`). The main command deliberately drops the proxy keys — the SQL-side
  `build_filter` used by writes has no relation resolver.
  - SQL: preCommand → `EXISTS` (existing §10.5 pushdown), main command → scalar `_id $in`;
  - MongoDB: both steps run as native commands — this fixes the previous **silent no-op**
    (`modifiedCount=0` with no warning) on the Mongo side;
  - Read-permission checks (R6 relation level, F3 sub-fields) run inside
    `relation_filter::plan` exactly as for queries.
- `needs_new_id` (autoincrement): schemas declaring `_id` with
  `strategy: "autoincrement"` never consume host-supplied IDs
  (`command/mutate/mod.rs`).

## 2. Autoincrement primary keys

- `build_insert_doc` (`command/write.rs`): for an autoincrement schema with no `_id`, the
  "no idPrefix" error is bypassed and `_id` is simply not injected — the SQL INSERT column
  whitelist comes from the document keys, so the database assigns the value;
- `dialect/write/insert.rs`: when the document has no `_id` column and the backend supports
  `RETURNING`, the INSERT gains `RETURNING _id` (PG/SQLite); MySQL relies on the host's
  last-insert-id;
- `computes/defaults.rs`: type-level implicit defaults (`int → 0`) are **not** applied to a
  missing autoincrement `_id` — a zero would masquerade as "already assigned";
- Archive schema derivation (`schema/registry.rs::archive_defn`) strips `_id.strategy`
  (archives copy source IDs explicitly; MySQL requires AUTO_INCREMENT columns to be keyed).

## 3. `$group by` one-relation paths

- `pipeline/group.rs::validate_by_key` accepts `relation.scalarField` paths when the relation
  is `one` and the leaf is a scalar field of the target schema; **`many` paths fail
  explicitly** (fan-out would break `$count:*` semantics);
- Mongo planning (`build_stages`, now taking `schema` + `registry`) emits
  `$lookup` + `$unwind {preserveNullAndEmptyArrays: true}` ahead of `$match`;
- SQL translation (`dialect/select/group_agg.rs::translate_group`, now taking `registry`)
  emits `LEFT JOIN g_<rel> ON g_<rel>.fk = t.local` and groups on `g_<rel>.<col>`; unmatched
  rows group under NULL, matching the Mongo `preserve` semantics;
- Node/Python hosts keep their DDL generators (byte-identical outputs), including the
  `CREATE [UNIQUE] INDEX` emission added alongside this batch.

## 4. Parity guardrails

- `cargo test -p rust-store-core` (158 tests) covers the §9.6 rewrite, group validation and
  dialect translation touched here;
- Scenario e2e (`manager-transaction`, 9 cases × 4 backends × 2 hosts) plus the
  `course-platform` regression suite (101 cases × 4 backends × 2 hosts) guard the behavior
  end to end; reports live under each host's `doc/test-eval/`.

## 5. Planned

Declarative schema migration (whitelist: add table / add column / type widening / add index;
everything else fails with `MIGRATION_UNSUPPORTED`) is designed and will be implemented in
the **hosts'** DDL modules (where schema→DDL already lives), keeping the Rust core
migration-free.
