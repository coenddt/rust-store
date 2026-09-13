---
name: "rust-store"
description: "Rust 单核心多后端数据引擎（workspace：core + core-node napi 绑定 + core-py PyO3 绑定），为 nodejs-store / py-store 提供 GQL 解析、权限、计算列、命令规划与 SQL 方言翻译，纯逻辑无 IO。调用场景：构建/调试 Rust 核心或绑定产物、改 core 的 GQL/方言/权限实现、跑 core 对拍（parity）测试时。"
---

# rust-store —— Rust 单核心 + 双绑定（多后端数据引擎）

## 1. 仓库定位

- `rust-store` 是 `nodejs-store` / `py-store` 共享的 **Rust 核心引擎**：`core` 实现语言无关纯逻辑，`core-node`（napi-rs）与 `core-py`（PyO3）把它分别暴露给 Node.js 与 Python，**双端语义天然一致**。
- 端到端链路：**GQL → `core` 解析/规划 → Mongo 命令 JSON（`find`/`aggregate`/`countDocuments`/…）→ `core/src/dialect` 纯函数翻译 → 各后端原生查询（Mongo 聚合 / MySQL·SQLite·PG 参数化 SQL）**；驱动返回的平铺 JOIN 行经 `dialect::row` 还原为嵌套 Mongo 文档。
- 关键分工（`core/src/lib.rs` 与各绑定 README 反复声明）：
  - **core 不持有任何数据库驱动、不做任何 IO**；Command 由 Host 执行。
  - 绑定层只做 **JSON in → JSON out** 的转换与转发，不执行 IO。
  - 时钟与随机源在 Host：`now` / `newId` 由调用方传入（core 无时钟、无随机源，便于对拍）。
- workspace 成员（根 `Cargo.toml`）：`core/`（`rust-store-core`）、`core-node/`（`rust-store-node`）、`core-py/`（`rust-store-py`）；统一使用根 `target/` 与根 `Cargo.lock`。三个 crate 均 `publish = false`（不经 crates.io 发布，绑定分别走 npm / PyPI）。
- 消费方：`nodejs-store` 依赖 npm 包 `rust-store-node`；`py-store` 依赖 PyPI 包 `rust-store-py`。

## 2. 安装 / 依赖

运行时（作为库被依赖时）：

```bash
npm i rust-store-node          # Node 绑定；平台原生二进制按 optionalDependencies 分包，装后自动匹配平台
pip install rust-store-py      # Python 绑定（maturin 构建 wheel）
```

开发期工具链：

- Rust stable（edition 2021），`cargo` / `clippy` / `rustfmt`。
- `core` 依赖：`serde_json`、`thiserror`（`core/Cargo.toml`）。
- `core-node` 依赖：`napi = "2"`（features `napi4`, `serde-json`）、`napi-derive`、`napi-build`；CLI `@napi-rs/cli`（devDeps）。
- `core-py` 依赖：`pyo3 = "0.29"`（features `extension-module`, `multiple-pymethods`）；构建用 `maturin>=1.7,<2.0`。
- 两个绑定 crate 都是 `crate-type = ["cdylib"]` 且 `[lib] test = false`（cdylib 测试二进制无宿主即 `STATUS_DLL_NOT_FOUND`），因此 `cargo test --workspace` 不会测绑定层，对拍一律在宿主侧。

## 3. 构建绑定产物

发布期（CI）用平台工具链构建：

```bash
# Node：core-node/ 目录
npx napi build --platform --release          # 产物 *.node（release 流程见 .github/workflows/release-npm.yml）
# Python：core-py/ 目录
maturin build --manifest-path core-py/Cargo.toml --release   # 产物 rust_store_py-*.whl
```

开发期手工构建（README 记载）：

```powershell
# Node 绑定
cargo build --manifest-path core-node/Cargo.toml
Copy-Item core-node/target/debug/rust_store_node.dll core-node/dist/rust-store-node.node -Force
# Python 绑定
cargo build --manifest-path core-py/Cargo.toml
Copy-Item core-py/target/debug/rust_store_py.dll core-py/dist/rust_store_py.pyd -Force
```

> Python 侧更推荐的开发流是 `python -m maturin develop --manifest-path core-py/Cargo.toml`（py-store 的 `core.py` 加载失败提示用的正是这条命令）。
> `dist/` 产物仅供本仓库测试与相邻 Host 调试，**不随包发布**。

## 4. 快速上手

### Node（`core-node`，camelCase）

```js
const { Registry } = require('rust-store-node');

const reg = new Registry();
reg.register({ name: 'User', collection: 'users', fields: { name: { type: 'string' } }, relations: {} });

const plan = reg.planQuery('User{name}', {}, null); // → Mongo 命令 JSON
```

同步计算列回调经 `setFn` 注册；异步计算列（`asyncFn`）走 `prepareQuery` / `stripQuery` 两段式。

### Python（`core-py`，snake_case）

```python
from rust_store_py import Registry

reg = Registry()
reg.register({"name": "User", "collection": "users", "fields": {"name": {"type": "string"}}, "relations": {}})

plan = reg.plan_query("User{name}", {}, None)  # → Mongo 命令 JSON（dict）
```

同步计算列经 `set_fn` 注册；`asyncFn` 同样两段式。**错误一律以异常（`PyErr`）抛出**，不会以返回值混进结果 dict。

### 系统上下文

- Node：`const { Registry, systemContext } = require('rust-store-node')`，`systemContext()` → `{ internal: true }`。
- Python：`rust_store_py.system_context()`（模块级函数，`core-py/src/lib.rs` 的 `#[pymodule]` 注册）。

用于区分「系统内部调用」与「未传上下文（`require_context` 开启时报错）」。

## 5. 核心 API 清单（Node 名 / Python 名逐一对应，均**真实存在**）

来源：`core-node/index.d.ts`（napi 自动生成）与 `core-py/src/lib.rs` + `core-py/src/methods/*.rs`。

### 5.1 生命周期与 schema

| Node | Python | 存在 | 说明 |
| --- | --- | --- | --- |
| `new Registry()` | `Registry()` | ✅ | 显式注册表实例（不再是模块级全局 `_schemas`） |
| `register(defn)` | `register(defn)` | ✅ | 注册 schema，自动派生 `<Name>Deleted` 归档表 |
| `has(name)` / `list()` | `has(name)` / `list()` | ✅ | |
| `setFn(fnRef, cb)` / `clearFns()` | `set_fn(fn_ref, cb)` / `clear_fns()` | ✅ | 同步计算列回调注册 |
| `setRequireContext(bool)` / `requireContext()` | `set_require_context(bool)` / `require_context()` | ✅ | fail-secure 开关（默认关闭） |

> 绑定层 Registry **没有** `get(name)`：Node 与 Python 均只暴露 `register`/`has`/`list`（Host 侧 `schema.get` 由各宿主包自行维护注册表字典）。

### 5.2 查询规划（读路径）

`buildPipeline`/`build_pipeline`、`planQuery`/`plan_query`、`planQueryOne`/`plan_query_one`、`planQueryWithCount`/`plan_query_with_count`、`resolvePage`/`resolve_page`、`restoreSortOrder`/`restore_sort_order`、`planExists`/`plan_exists`、`planCount`/`plan_count`、`sortsByRelation`/`sorts_by_relation` —— 全部 ✅。

要点：`planQueryOne` 在未显式 `$limit` 时强制下推 `$limit(1)`；`planQueryWithCount` 的 `total` 由 Host 执行 `countCommand` 后回喂。

### 5.3 写路径规划

`planInsert`/`plan_insert`、`planInsertMany`/`plan_insert_many`、`planUpdate`/`plan_update`、`planUpdateMany`/`plan_update_many`、`planRemove`/`plan_remove`、`planArchiveDocs`/`plan_archive_docs`、`planUpsert`/`plan_upsert`、`planMutation`/`plan_mutation`、`applyWriteDefaults`/`apply_write_defaults` —— 全部 ✅。

要点：
- `planUpdate` / `planRemove` 的 creator 权限需探针时返回 `{"needsProbe": cmd}`，Host 执行探针后携 `probeFound`/`probeDoc` 重入得 `{"command": cmd}`。
- `planUpdateMany` 对 guest / 无写授权直接拒绝，**不走** creator 探针。
- `planRemove` 返回归档 `findCommand` + `deleteCommand`；归档写 `<collection>_deleted` 并补 `deletedAt`。
- `planMutation` 展开为有序步骤序列，父子依赖用 `{{step.<N>._id}}` 占位符表达，由 Host 依次执行并回填。
- 所有 plan 方法尾参均为可选 `routeOverride` / `route_override`（`{source, namespace}`）。

### 5.4 权限

`canRead`/`can_read`、`canWrite`/`can_write`、`shouldInjectOwner`/`should_inject_owner`、`mergeOwnerCondition`/`merge_owner_condition`、`readableFields`/`readable_fields`、`readableRelations`/`readable_relations`、`writableFields`/`writable_fields`、`filterWritableData`/`filter_writable_data` —— 全部 ✅。

### 5.5 计算列 / 后处理

`processNode`/`process_node`、`asyncFnRefs`/`async_fn_refs`、`injectDepends`/`inject_depends`、`stripDepInjected`/`strip_dep_injected`、`prepareQuery`/`prepare_query`（两段式阶段一，返回 `{items, fnRefs}`）、`stripQuery`/`strip_query`（阶段三）—— 全部 ✅。

### 5.6 数据源 / 方言 / 联邦

| Node | Python | 存在 | 说明 |
| --- | --- | --- | --- |
| `resolveDatasource(schemaName, config)` | `resolve_datasource(...)` | ✅ | 返回 `"mongo"`/`"mysql"`/`"postgres"`/`"sqlite"` |
| `schemaDatasource(schemaName)` | `schema_datasource(...)` | ✅ | 未声明 → `null`（语义 `default`） |
| `dialectTranslate(backend, cmd)` | `dialect_translate(...)` | ✅ | Mongo 命令 JSON → SQL 语句序列 |
| `restoreRows(shape, rows)` | `restore_rows(...)` | ✅ | 平铺行 → 嵌套文档 |
| `schemaFromRows(rows, backend)` | `schema_from_rows(...)` | ✅ | introspection 行 → schemaJSON |
| `mergeSchema(base, overlay)` | `merge_schema(...)` | ✅ | 物理结构 + 本地 overlay |
| `planFederated(gql, params, ctx, dsConfig)` | `plan_federated(...)` | ✅ | 拆分「各源命令 + 内存 join 边」；返回含 `degraded` |
| `mergeFederated(plan, results)` | `merge_federated(...)` | ✅ | `results` 必须与 `plan.sources` **同序同长** |

模块级函数：`systemContext()`（Node）/ `system_context()`（Python）—— ✅。

**不存在的 API（勿臆造）**：core 无 `aggregate` 直通、无 `parseGql`/`parse_gql` 公开方法（解析内经 `pipeline::parse_gql`）、无任何 IO/驱动/执行方法。

## 6. GQL 查询语法（`core/src/pipeline/`）

```text
ModelName($condition:@c0,$sort:@s1,$skip:@sk,$limit:@l1) {
  field1, field2, obj.subField,
  RelationName($condition:@c2,$sort:@s3,$limit:@l2) { field3, NestedRelation { field4 } }
}
```

- 值用 `@key` 引用 params 对象；关系名后「参数列表 + 选择集」可同时出现（曾修复 `Rel($limit:@l){f}` 的解析歧义）。
- 关系在 schema 声明（`type: "many" | "one"`），由 `pipeline/lookup.rs` 构建 `$lookup`/`$addFields` —— **不要手写 `$lookup`**。
- 递归保护：`MAX_DEPTH = 10`、`MAX_PAGINATED_DEPTH = 4`（`pipeline/mod.rs`）。
- **`$pipeline` 直通已移除**：GQL 中出现即**显式报错**（`pipeline/parse.rs`），不静默忽略。

### 根级 `$group` / `$having`（已实现）

入口（`pipeline/group.rs`）：`Course($condition:@c0, $group:@g0, $having:@h0, $sort:@s0, $skip:@sk, $limit:@l0){ … }`

- 规格：`{ "by": ["status","meta.level"], "agg": { "n": {"$count":"*"}, "total": {"$sum":"price"} } }`。
- 算子白名单（`$group` 与 SQL 翻译两侧一致）：`$count` / `$sum` / `$avg` / `$min` / `$max`；`$count:"*"` 表示行数。
- 固定执行序（§9.3）：`$condition`(WHERE) → `$group`(GROUP BY) → `$having`(HAVING) → `$sort` → `$skip/$limit` → 投影；有 `$group` 时排序/分页作用于**分组结果**，`$sort` 键域 = `by` 键 ∪ `agg` 别名。
- `$having` 必须与 `$group` 同用，否则报错。
- 校验：`by` 仅标量域（可含 object 点号路径），关系/数组/裸对象/schema 外字段 → `Err`；`agg` 仅本表标量字段（关系/数组/对象/点号路径/表外字段 → `Err`）。
- SQL 翻译见 `dialect/select/group_agg.rs`：Mongo `$group`（`_id` + 累积器）→ `GROUP BY` + `SELECT 聚合列`；`$group` 后的 `$match` → `HAVING`；全表单组（`by` 省略 / `[]`）→ 无 `GROUP BY`。

### 计算列（`core/src/computes/`）

- `fn`（同步，经绑定 `set_fn` 注册回调）/ `asyncFn`（异步，Host 两段式）/ `agg`（关系聚合，**已归一，取代旧 `lookup`**）。
- `agg` 形态：`{"$count": "<关系名>"}` 或 `{"$sum"|"$avg"|"$min"|"$max": "<关系>.<字段>"}`；与 `fn`/`asyncFn` 互斥；空集语义 `$count → 0`，其余 `→ None`（nullable）。SQL 走派生表 `LEFT JOIN (… GROUP BY fk)`，Mongo 走 `$lookup` + `$addFields`。

## 7. 多后端 / 方言

- 后端：**MongoDB**（原生聚合）/ **MySQL**（参数化 SQL，`information_schema`）/ **SQLite**（`?` 参数化，`sqlite_master` + `PRAGMA`）/ **PostgreSQL**（`$n` 参数化，`RETURNING` 写后回读）。
- `dialect/` 子模块：`translate`（总入口）、`ir`（`RowShape`/`SqlStmt`）、`filter/`、`select/`（`find`/`aggregate`/`group_agg`/`lookup_join`）、`write/`（`insert`/`update`/`upsert`）、`row`（行还原）、`overlay`、`introspect`。
- datasource 注册：Host 传 `dsConfig = { "sources": { "<name>": "<kind>" } }`（`null` = 单源 Mongo）。
- SQL 同源跨 namespace 仍下推（qualified `JOIN`），Mongo 跨 db 剥离为内存 join（联邦）。
- 权限模型（`core/src/permission.rs`）：schema 级 `read`/`write` + 字段级 `field.read`/`field.write` + 关系级 `rel.read` + 计算列级 `comp.read`；`super_admin`/`admin`/`internal` 全放行；`guest` 无论 schema 配置均无写权限；`write: []` 空白名单 = 拒绝一切写；`creator` 伪角色按 `doc.createdBy == ctx.userId` 动态判定。
- 权限上下文 **显式入参**（`ctx`），不再是 JS 的隐式 `AsyncLocalStorage` —— 这是与旧 JS 实现的 P0 契约差异。

## 8. 测试与发布

```bash
# 1) core（纯 Rust，无宿主依赖）
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p rust-store-core

# 2) Node 绑定对拍（core-node/ 目录）
npm ci
npx napi build --platform        # 或 napi build
npm test                         # node --test "test/**/*.test.js"

# 3) Python 绑定对拍（仓库根）
cd core-py && pip install maturin pytest && maturin build --out dist && pip install --force-reinstall dist/*.whl
cd .. && python -m pytest core-py/test/parity_test.py -v

# 4) 黄金基准三侧复算
node tools/verify-fixtures.js
```

- core 测试：`core/tests/parity*.rs`（parity / parity_computes / parity_commands / parity_write / parity_fnfns / parity_dialect / parity_federation，另含 `guards.rs`、`pushdown_usecases.rs`、`regression_d_fixes.rs`、`route_override.rs`）。
- 绑定对拍：`core-node/test/{parity.test.js, dialect.smoke.test.js, t2q.skill.test.js}`、`core-py/test/parity_test.py`。
- 黄金基准：`fixtures/{pipeline,commands,computes,fnfns,federation,expected,host}/`。**无生成器**（原单体 JS 参考实现已退役，快照无源可再生）；`tools/verify-fixtures.js` 用三侧实现逐条重算并与冻结快照深比较，三侧全绿即「无 diff、可复现」。
- 压测：`stress/stress_core.py`。
- 发布（`.github/workflows/`）：
  - `ci.yml`：core（fmt + clippy + `cargo test -p rust-store-core`）、node-binding（napi debug 构建 + `npm test`）、py-binding（maturin wheel + `parity_test.py`）。
  - `release-npm.yml`：推 `v*` tag → 矩阵构建 `*.node` → `napi prepublish -t npm --skip-gh-release` 发平台子包 + 主包（`--provenance`）。
  - `release-pypi.yml`：推 `v*` tag → maturin-action 逐平台 `--find-interpreter` 出 wheel（**PyO3 未开 `abi3`，wheel 与 CPython 版本一一对应**；不产 sdist，因 `core-py` 依赖仓库外 path 依赖 `../core`）→ PyPI Trusted Publishing。

## 9. 常见坑

1. **`core` 无时钟、无随机源**：`now` / `newId(s)` 必须由 Host 传入，core 只按需消费；这保证跨语言可复现。
2. **`ctx` 是显式入参**：绑定层的每个 plan 方法都要传 `ctx`；不传 = 无上下文（默认放行，`require_context` 开启则报 `ERR_NO_CONTEXT`）。
3. **`require_context` 默认关闭（fail-open）**，为对齐 JS parity；需要 fail-secure 时 Host 启动即 `setRequireContext(true)`，内部调用传 `systemContext()` / `system_context()`。
4. **错误契约**：权限/守卫错误带**稳定前缀**（`ERR_PERM_PREFIX`、`ERR_NO_WRITE`、`ERR_NO_BATCH_WRITE`、`ERR_NO_CONTEXT`，见 `core/src/command/mod.rs`），Host 按前缀映射为 `PermissionError`。Python 绑定必须让错误经 `PyErr` **抛出**，不能混进返回 dict。
5. **R4：空条件批量写一票否决**：`updateMany` / `remove` 条件为 `{}`、`null` 或空逻辑组（`{"$and":[]}` / `{"$or":[]}`）一律视为无条件 → **显式拒绝**，绝不落全表（`command/mutate/*`）。
6. **`__present` 是 SQL 内部哨兵列**（`dialect/write/mod.rs`）：用于区分「显式 null（有键）」与「缺失（无键）」，由翻译层注入/消费；PostgreSQL `ON CONFLICT DO UPDATE` 中的引用须限定目标表（否则报 `column reference "__present" is ambiguous`）。
7. **U1~U4 全局显式报错**：数组字段直接过滤（U1）、对象深度等值过滤（U2）、对象点号路径过滤（U3）/排序（U4）在所有后端统一报错；空逻辑组也报错（`types.rs` 校验）。关系路径排序不等于对象点号排序，不受 U4 影响。
8. **timestamps 值校验**：仅接受 `true`/`false`/`"ms"`/`"s"`（缺省毫秒），非法值**注册即报错**；单位换算由 Host 时钟负责。
9. **联邦 degraded 不阻断**：无法下推的跨源分页/排序产出结构化事件 `{code, layer, message, hint}`（`planFederated` 返回的 `plan.degraded`），Host 应接入自动反馈闭环。
10. **不可翻译必须显式**（项目铁律）：translation 遇到无法安全处理的组合必须报错或产出 `unsupported` + warning，绝不产出「缺少该段」的 SQL。
11. **绑定层无 Rust 单测**：`core-node` / `core-py` 均 `[lib] test = false`，改绑定后必须跑宿主侧对拍（`npm test` / `pytest parity_test.py`），`cargo test --workspace` 不会覆盖它们。
12. **`planFederated` 的 `results` 必须与 `plan.sources` 同序同长**，否则 `mergeFederated` 结果错位。

## 10. 相关 skill / 文档

- 宿主侧 skill：`py-store`、`nodejs-store`（各自仓库 `.trae/skills/`）。
- 本仓库文档：`doc/code-review/2026/09/`、`doc/test-eval/2026/09/`、`doc/fix-plan/2026-09/`（含 `缺陷分层决策-共享核心修复主案.md`）、`doc/2026-09-12-测试报告.md`、`doc/2026-09-12-压测报告.md`。

## 11. 文档与代码不一致（实测差异）

- **README「绑定产物对接」与两个绑定 README 给出的开发期路径是 `core-node/target/debug/...` / `core-py/target/debug/...`**，但根 `Cargo.toml` 注释明确「workspace 统一使用根目录 `target/`」。因此实际产物更可能在**仓库根** `target/debug/` 下；手工 `Copy-Item` 前请以实际构建输出（或直接用 `npx napi build` / `maturin develop`）为准。
- **root `README` 提到 `src/nodejs-store`**：该路径是本仓库早期规划名，实际 Node 宿主为**独立仓库 `../nodejs-store`**（`rust-store/README.md` 末尾链接指向同级的两个宿主仓库）。
- **fixtures 无生成器**：`tools/` 只有 `verify-fixtures.js` 与 `test-fns.js`，没有 `gen-*-fixtures.js`（说明见 `verify-fixtures.js` 头部），不要尝试"从当前 core 反向生成"基准。
