# rust-store

**多后端数据访问的单一 Rust 核心引擎 —— GQL 解析、权限校验、计算列、命令规划与 SQL 方言翻译，经原生绑定同时服务 Node.js 与 Python。**

![npm version](https://img.shields.io/npm/v/rust-store-node)
![PyPI version](https://img.shields.io/pypi/v/rust-store-py)
![license](https://img.shields.io/badge/license-MIT-blue)
![rust](https://img.shields.io/badge/rust-stable-orange)
![bindings](https://img.shields.io/badge/bindings-napi--rs%20%7C%20PyO3-blueviolet)

> English docs: [README.md](README.md)

`rust-store` 是 [`nodejs-store`](https://github.com/coenddt/nodejs-store) 与 [`py-store`](https://github.com/coenddt/py-store) 共享的引擎。它**不持有任何数据库驱动、不做任何 IO**：把一条查询（GQL）或一次写入编译为后端无关的**命令**（MongoDB 命令 JSON）交给宿主执行，并把命令翻译为 MySQL / PostgreSQL / SQLite 的参数化 SQL。

---

## 目录

- [它是什么](#它是什么)
- [何时使用](#何时使用)
- [与 nodejs-store / py-store 的关系](#与-nodejs-store--py-store-的关系)
- [workspace 布局](#workspace-布局)
- [安装](#安装)
- [快速上手](#快速上手)
- [API 参考](#api-参考)
- [GQL 能力](#gql-能力)
- [后端与方言](#后端与方言)
- [权限模型](#权限模型)
- [测试与对拍](#测试与对拍)
- [边界与常见坑](#边界与常见坑)
- [FAQ](#faq)
- [相关项目](#相关项目)

---

## 它是什么

一门用 Rust 写一次、所有宿主语言共用的多后端数据引擎。

- **查询语言**：GQL —— 类 MongoDB 的树状语法，支持关系、分页、分组与聚合谓词。
- **规划**：GQL + params → **MongoDB 命令 JSON**（`find` / `aggregate` / `countDocuments` / 写命令）。
- **翻译**：MongoDB 命令 JSON → MySQL、PostgreSQL、SQLite 的**参数化 SQL**（纯函数）。
- **结果还原**：平铺 JOIN 行 → 嵌套文档。
- **横切关注点**：schema 注册表、权限引擎（schema / 字段 / 关系 / 计算列级）、计算列（`fn` / `asyncFn` / `agg`）、软删除归档规划、跨数据源联邦规划。

**核心原则**（全代码库强制）：

1. **无 IO、无时钟、无随机源。** core 永不打开连接、永不读取系统时钟。`now` 与 `newId(s)` 一律由宿主传入 —— 这正是引擎确定、跨语言可复现的根源。
2. **显式优于静默。** 任何无法安全翻译的东西都会报错，或产出结构化的 `unsupported` + warning。引擎绝不产出悄悄缺一段的 SQL。
3. **JSON in，JSON out。** 绑定层只做转换与转发，不新增行为，因此 Node.js 与 Python 不可能语义漂移。

### 与其他具体项目的区别

仅为定位说明，基于这些项目在撰写时的公开文档 —— 请以你自己的需求为准去核实。

- **对比 `sqlx` / Diesel / SeaORM** —— 它们是直接访问 SQL 数据库的 Rust 数据库工具集与 ORM。`rust-store-core` 从不开连接：它只规划命令并翻译方言，产出的命令 JSON 由 Node.js 或 Python 宿主执行。正因如此，同一引擎才能以完全相同的语义同时服务两种宿主。
- **对比「把这一层写两遍」** —— 通常的替代做法是写一份 JavaScript 实现再加一份 Python 重写实现，二者会随时间产生漂移。而这里是一个 Rust core 通过纯 JSON 桥被绑定两次（`napi-rs`、`PyO3`），并由 `core/tests/parity*.rs` 以及 `fixtures/` 中的黄金夹具强制两个绑定保持一致。
- **对比 `transports` 式「一个 core、多个绑定」的项目** —— 跨绑定共享一个 Rust core，是序列化/传输层已被验证的模式。`rust-store` 把这一模式应用到了*数据访问语义*上：一套 GQL、一个权限引擎与四种 SQL/Mongo 方言，落在两个语言绑定之后。
- **对比「在宿主语言里做」** —— 在 JavaScript *和* Python 里各实现一遍 GQL 解析、权限与四种 SQL 方言，意味着两条代码路径、两处 bug 面、两套边界情况。Rust core 让边界变得明确：纯逻辑集中在一处，IO 留在各宿主中。

## 何时使用

- **你在构建或调试 Node.js / Python 宿主**（[`nodejs-store`](https://github.com/coenddt/nodejs-store) / [`py-store`](https://github.com/coenddt/py-store)）—— GQL 解析、权限与方言翻译真正住在这个仓库里。
- **你想在 Rust 服务里用同一套查询方言。** `rust-store-core` 就是一个普通 Rust 库：注册 schema、规划查询、翻译成 SQL，再把命令交给自己的驱动执行。
- **你在为另一种语言写宿主。** core 与语言无关；`core-node`（napi-rs）与 `core-py`（PyO3）就是 JSON 命令契约的两个落地范例。
- **你需要 Node 服务与 Python 服务之间保证一致。** 二者消费同一个引擎，同一条查询在两边行为相同。
- **你想把权限与计算列逻辑从应用代码里挪出来**，写进 schema 并在规划期强制执行。
- **你在构建 AI / 自然语言查询层。** 规划是纯函数（`gql → plan`），模型的产出可以在碰到数据库之前先编译、检查、拒绝。

## 与 nodejs-store / py-store 的关系

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

- **core** 拥有全部纯逻辑。
- **宿主** 拥有驱动 IO、回调（同步计算列、异步计算列）与占位符替换。
- **绑定** 是薄薄的 JSON 桥。

若你只想*使用*数据层，安装 `nodejs-store` 或 `storepy` 即可 —— 无需直接依赖本仓库。

## workspace 布局

| 目录 | crate / 包 | 说明 |
| --- | --- | --- |
| `core/` | `rust-store-core` | 语言无关核心：GQL / 权限 / 计算列 / 命令规划。纯逻辑、无 IO。 |
| `core-node/` | `rust-store-node` | Node 绑定（napi-rs）→ `dist/rust-store-node.node`。以 `rust-store-node` 发布到 npm。 |
| `core-py/` | `rust-store-py` | Python 绑定（PyO3）→ `dist/rust_store_py.pyd`。以 `rust-store-py` 发布到 PyPI。 |

workspace 统一使用根目录 `target/` 与根 `Cargo.lock`。三个 crate 均 `publish = false`（不经 crates.io 发布，绑定分别走 npm 与 PyPI）。

值得了解的内部模块：`pipeline/`（GQL 解析 → AST → `$lookup`/`$group` 构建）、`command/`（query/count/write/mutation 规划器）、`dialect/`（filter、select、write、行还原、introspection、overlay）、`computes/`（sync / async / agg）、`permission.rs`、`federation/`、`schema/`。

## 安装

绑定产物（宿主所依赖的）：

```bash
npm i rust-store-node          # Node 绑定；平台原生二进制按 optionalDependencies 分包
pip install rust-store-py      # Python 绑定（maturin 构建 wheel）
```

Rust 库（path 依赖 —— crate 未发布到 crates.io）：

```toml
[dependencies]
rust-store-core = { path = "path/to/rust-store/core" }
```

开发期工具链：Rust stable（edition 2021），`cargo` / `clippy` / `rustfmt`；`core-node` 用 `napi-rs` CLI；`core-py` 用 `maturin >= 1.7, < 2.0`。

本地构建绑定：

```bash
# Node 绑定（core-node/）
npx napi build --platform --release          # release 流程: .github/workflows/release-npm.yml

# Python 绑定（core-py/）
maturin build --manifest-path core-py/Cargo.toml --release
```

## 快速上手

### Node.js（`core-node`，camelCase）

```js
const { Registry } = require('rust-store-node');

const reg = new Registry();
reg.register({ name: 'User', collection: 'users', fields: { name: { type: 'string' } }, relations: {} });

const plan = reg.planQuery('User{name}', {}, null);   // → MongoDB 命令 JSON
```

同步计算列回调经 `setFn` 注册；异步计算列（`asyncFn`）走 `prepareQuery` / `stripQuery` 两段式。

### Python（`core-py`，snake_case）

```python
from rust_store_py import Registry

reg = Registry()
reg.register({"name": "User", "collection": "users", "fields": {"name": {"type": "string"}}, "relations": {}})

plan = reg.plan_query("User{name}", {}, None)  # → MongoDB 命令 JSON（dict）
```

同步计算列经 `set_fn` 注册；`asyncFn` 同样走两段式。**错误一律以 Python 异常（`PyErr`）抛出**，不会以返回值混进结果 dict。

### 系统上下文

`ctx` 是每个 plan 方法上的显式入参。`{ internal: true }` 标记一次*系统调用*（权限引擎全放行、不注入 owner 条件），与 `undefined`/`None`（无上下文）在语义上不同。

- Node：`const { Registry, systemContext } = require('rust-store-node')` → `systemContext()` 返回 `{ internal: true }`。
- Python：`rust_store_py.system_context()`（模块级函数）。

## API 参考

Node 名与 Python 名一一对应（camelCase ↔ snake_case）。下列方法在两个绑定中均存在。

### 生命周期与 schema

| Node | Python | 说明 |
| --- | --- | --- |
| `new Registry()` | `Registry()` | 显式注册表实例（不再是模块级全局） |
| `register(defn)` | `register(defn)` | 注册 schema；自动派生 `<Name>Deleted` 归档表 |
| `has(name)` / `list()` | `has(name)` / `list()` | |
| `setFn(fnRef, cb)` / `clearFns()` | `set_fn(fn_ref, cb)` / `clear_fns()` | 同步计算列回调注册 |
| `setRequireContext(bool)` / `requireContext()` | `set_require_context(bool)` / `require_context()` | fail-secure 开关（默认关闭） |

> 绑定层 `Registry` **没有** `get(name)` —— Host 自行维护 schema 字典。

### 读路径

`buildPipeline` / `build_pipeline`、`planQuery` / `plan_query`、`planQueryOne` / `plan_query_one`、`planQueryWithCount` / `plan_query_with_count`、`resolvePage` / `resolve_page`、`restoreSortOrder` / `restore_sort_order`、`planExists` / `plan_exists`、`planCount` / `plan_count`、`sortsByRelation` / `sorts_by_relation`。

要点：`planQueryOne` 在未显式给 `$limit` 时强制下推 `$limit(1)`；`planQueryWithCount` 的 `total` 由 Host 执行 `countCommand` 后回喂。

### 写路径

`planInsert` / `plan_insert`、`planInsertMany` / `plan_insert_many`、`planUpdate` / `plan_update`、`planUpdateMany` / `plan_update_many`、`planRemove` / `plan_remove`、`planArchiveDocs` / `plan_archive_docs`、`planUpsert` / `plan_upsert`、`planMutation` / `plan_mutation`、`applyWriteDefaults` / `apply_write_defaults`。

要点：

- `planUpdate` / `planRemove` 的 creator 权限需探针时返回 `{ "needsProbe": cmd }`；Host 执行探针后携 `probeFound` / `probeDoc` 重入，得 `{ "command": cmd }`。
- `planUpdateMany` 对 guest / 无写授权直接拒绝，且**不走** creator 探针。
- `planRemove` 返回归档 `findCommand` + `deleteCommand`；归档文档写入 `<collection>_deleted` 并补 `deletedAt` 字段。
- `planMutation` 展开为有序步骤序列，父子依赖用 `{{step.<N>._id}}` 占位符表达，由 Host 依次执行并回填。
- 每个 plan 方法尾参均为可选 `routeOverride` / `route_override`（`{source, namespace}`）。

### 权限

`canRead` / `can_read`、`canWrite` / `can_write`、`shouldInjectOwner` / `should_inject_owner`、`mergeOwnerCondition` / `merge_owner_condition`、`readableFields` / `readable_fields`、`readableRelations` / `readable_relations`、`writableFields` / `writable_fields`、`filterWritableData` / `filter_writable_data`。

### 计算列与后处理

`processNode` / `process_node`、`asyncFnRefs` / `async_fn_refs`、`injectDepends` / `inject_depends`、`stripDepInjected` / `strip_dep_injected`、`prepareQuery` / `prepare_query`（两段式阶段一，返回 `{items, fnRefs}`）、`stripQuery` / `strip_query`（阶段三）。

### 数据源、方言、联邦

| Node | Python | 说明 |
| --- | --- | --- |
| `resolveDatasource(schemaName, config)` | `resolve_datasource(...)` | 返回 `"mongo"` / `"mysql"` / `"postgres"` / `"sqlite"` |
| `schemaDatasource(schemaName)` | `schema_datasource(...)` | 未声明 → `null`（语义：`default`） |
| `dialectTranslate(backend, cmd)` | `dialect_translate(...)` | MongoDB 命令 JSON → SQL 语句序列 |
| `restoreRows(shape, rows)` | `restore_rows(...)` | 平铺行 → 嵌套文档 |
| `schemaFromRows(rows, backend)` | `schema_from_rows(...)` | introspection 行 → schemaJSON |
| `mergeSchema(base, overlay)` | `merge_schema(...)` | 物理结构 + 本地 overlay |
| `planFederated(gql, params, ctx, dsConfig)` | `plan_federated(...)` | 把一条 GQL 拆成「各源命令 + 内存 join 边」；返回值含 `degraded` |
| `mergeFederated(plan, results)` | `merge_federated(...)` | `results` 必须与 `plan.sources` 同序同长 |

模块级函数：`systemContext()`（Node）/ `system_context()`（Python）。

**不存在的 API（勿臆造）**：core 无 `aggregate` 直通、无公开的 `parseGql` / `parse_gql` 方法（解析内经 `pipeline::parse_gql`）、无任何 IO / 驱动 / 执行方法。

## GQL 能力

```text
ModelName($condition:@c0,$sort:@s1,$skip:@sk,$limit:@l1) {
  field1, field2, obj.subField,
  RelationName($condition:@c2,$sort:@s3,$limit:@l2) { field3, NestedRelation { field4 } }
}
```

- 值以 `@key` 从 params 对象引用。
- 关系在 schema 声明（`type: "many" | "one"`），由引擎编译为 `$lookup` / `$addFields` —— **不要手写 `$lookup`**。
- 递归保护：`MAX_DEPTH = 10`、`MAX_PAGINATED_DEPTH = 4`。
- **`$pipeline` 直通已移除** —— 出现即为显式解析错误，不静默忽略。

### 根级 `$group` / `$having`

```text
Course($condition:@c0, $group:@g0, $having:@h0, $sort:@s0, $skip:@sk, $limit:@l0) { status, n, total }
```

- 规格：`{ "by": ["status","meta.level"], "agg": { "n": {"$count":"*"}, "total": {"$sum":"price"} } }`。
- 算子白名单（`$group` 与 SQL 翻译两侧一致）：`$count` / `$sum` / `$avg` / `$min` / `$max`；`$count: "*"` 表示行数。
- 固定执行序：`$condition`(WHERE) → `$group`(GROUP BY) → `$having`(HAVING) → `$sort` → `$skip`/`$limit` → 投影。有 `$group` 时，排序/分页作用于**分组结果**，`$sort` 键域 = `by` 键 ∪ `agg` 别名。
- `$having` 必须与 `$group` 同用，否则报错。
- 校验：`by` 仅标量域（可含 object 点号路径）；关系/数组/裸对象/schema 外字段 → `Err`。`agg` 仅本表标量字段（关系/数组/对象/点号路径/表外字段 → `Err`）。
- SQL 翻译（`dialect/select/group_agg.rs`）：Mongo `$group`（`_id` + 累积器）→ `GROUP BY` + 聚合列；`$group` 后的 `$match` → `HAVING`；全表单组（`by` 省略 / `[]`）→ 无 `GROUP BY`。

### 关系聚合谓词（§9.6）

以「对关系的聚合」过滤父行 —— 一次不扇出的 semi-join：

- 简写：`{ "<relation>": { "$exists": true|false } }`、`{ "$count": { "$of"?: field, "<cmp>": value } }`、`{ "$sum"|"$avg"|"$min"|"$max": { "$of": field, "<cmp>": value } }`，可与 `$filter` 块组合。
- 主形式：`{ filter?, agg, having }`。
- 比较算子：`$gt` / `$gte` / `$lt` / `$lte` / `$eq` / `$ne`。
- `$not` 包裹单个关系谓词，或 `$exists: false`，产出 anti-join。
- SQL：`EXISTS` / `NOT EXISTS`；MongoDB：哨兵代理键。
- 仅一级关系；`orders.items.price` → `Err`。调用方不可读的关系 → `Err`，绝不静默当 `false`。不可与根级 `$group` 并用。

### 计算列

在 schema 中声明；三种形态：

| 形态 | 求值位置 | 说明 |
| --- | --- | --- |
| `fn` | 宿主（同步，经 `set_fn`） | 依赖字段自动注入投影 |
| `asyncFn` | 宿主（异步，两段式） | 同样的注入，查询返回后再求值 |
| `agg` | **引擎内联** | `{"$count": "<relation>"}` 或 `{"$sum"|"$avg"|"$min"|"$max": "<relation>.<field>"}` |

`agg` 与 `fn` / `asyncFn` 互斥。空集语义：`$count → 0`，其余 → `None`（nullable）。SQL 走派生表 `LEFT JOIN (… GROUP BY fk)`；MongoDB 走 `$lookup` + `$addFields`。

## 后端与方言

| 后端 | 说明 |
| --- | --- |
| **MongoDB** | 原生聚合管道。 |
| **MySQL** | 参数化 SQL，`information_schema` introspection。 |
| **SQLite** | 参数化 SQL（`?`），`sqlite_master` + `PRAGMA` introspection。 |
| **PostgreSQL** | 参数化 SQL（`$n`），`RETURNING` 写后回读。 |

datasource 注册：Host 传 `dsConfig = { "sources": { "<name>": "<kind>" } }`（`null` = 单源 Mongo）。SQL 同源跨 namespace 仍下推（qualified `JOIN`）；Mongo 跨 db 关系剥离为内存联邦。

跨后端翻译：根级 `$group` / `$having` → `GROUP BY` / `HAVING`；`$count`/`$sum`/`$avg`/`$min`/`$max` 白名单；关系聚合谓词 → `EXISTS` / `NOT EXISTS`（`WHERE EXISTS (SELECT 1 … GROUP BY fk HAVING …)`）；关系滚动 `agg` 计算列 → 派生表 `LEFT JOIN (… GROUP BY fk)`；关系每父 top-N（关系块内 `$sort`/`$skip`/`$limit`）→ `ROW_NUMBER() OVER (PARTITION BY fk ORDER BY …)`。

**仅 Mongo 可执行 / SQL 侧显式不支持**：`$group.by` 中的 object 点号路径（Mongo 可执行，SQL 无法映射为标量列 → `Err`）；无法映射的根级 `$sort` 键（未知字段 / object·array 字段 / 关系名本身 / 无对应关系下钻）不下推到 SQL（告警 + Host 侧兜底排序），而 Mongo 照常执行。

## 权限模型

schema 级 `read` / `write`、字段级 `field.read` / `field.write`、关系级 `rel.read`、计算列级 `comp.read`。

- `super_admin` / `admin` / `internal` 全放行。
- `guest` 无论 schema 配置如何均无写权限。
- `write: []`（空白名单）= 拒绝一切写。
- `creator` 为伪角色，按 `doc.createdBy == ctx.userId` 动态判定。
- 权限上下文是**显式入参**（`ctx`）—— 这是与旧隐式 `AsyncLocalStorage` 风格设计的刻意差异。

面向 AI 查询宿主的接入守卫：`timestamps` 值校验（仅 `true` / `false` / `"ms"` / `"s"`，非法值注册即报错）与联邦 `degraded` 事件（`{code, layer, message, hint}`，见 `plan.degraded`），令无法下推的跨源分页/排序绝不静默阻断查询。守卫测试见 `core/tests/guards.rs`。

## 测试与对拍

```bash
# 1) core（纯 Rust，无宿主依赖）
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p rust-store-core

# 2) Node 绑定（core-node/）
npm ci
npx napi build --platform
npm test

# 3) Python 绑定（仓库根）
cd core-py && pip install maturin pytest && maturin build --out dist && pip install --force-reinstall dist/*.whl
cd .. && python -m pytest core-py/test/parity_test.py -v

# 4) 黄金基准三侧复算
node tools/verify-fixtures.js
```

- core 测试：`core/tests/parity*.rs`（parity、parity_computes、parity_commands、parity_write、parity_fnfns、parity_dialect、parity_federation），另含 `guards.rs`、`pushdown_usecases.rs`、`regression_d_fixes.rs`、`route_override.rs`。
- 绑定对拍：`core-node/test/{parity.test.js, dialect.smoke.test.js, t2q.skill.test.js}` 与 `core-py/test/parity_test.py`。
- 黄金基准：`fixtures/{pipeline,commands,computes,fnfns,federation,expected,host}/`。**无生成器** —— `tools/verify-fixtures.js` 用三侧实现逐条重算并与冻结快照深比较；三侧全绿即「无 diff、可复现」。
- **绑定 crate 没有 Rust 单测**（`[lib] test = false`），故 `cargo test --workspace` 不覆盖它们 —— 改绑定后必须跑宿主侧对拍。

## 边界与常见坑

1. **`core` 无时钟、无随机源。** `now` / `newId(s)` 必须由宿主传入，这保证跨语言结果可复现。
2. **`ctx` 是显式入参**，每个 plan 方法都要传。省略即「无上下文」（默认放行；`require_context` 开启时报错）。
3. **`require_context` 默认关闭（fail-open）**，为对齐原 JS 实现。需要 fail-secure 时在宿主启动时打开，内部调用传 `systemContext()`。
4. **稳定错误前缀**：`ERR_PERM_PREFIX`、`ERR_NO_WRITE`、`ERR_NO_BATCH_WRITE`、`ERR_NO_CONTEXT`（`core/src/command/mod.rs`）。Host 按前缀映射为各自的错误类型；Python 必须以 `PyErr` 抛出，绝不能作为返回值返回。
5. **空条件批量写被否决**：`updateMany` / `remove` 条件为 `{}`、`null` 或空逻辑组（`{"$and":[]}` / `{"$or":[]}`）时视为无条件，显式拒绝 —— 绝不落全表。
6. **`__present` 是 SQL 内部哨兵列**：用于区分「显式 null（有键）」与「缺失（无键）」，由翻译层注入并消费；PostgreSQL `ON CONFLICT DO UPDATE` 中的引用须限定目标表，否则报 `column reference "__present" is ambiguous`。
7. **U1–U4 是全局错误**：数组字段直接过滤（U1）、对象深度等值过滤（U2）、对象点号路径过滤（U3）与对象点号路径排序（U4）在所有后端统一报错；空逻辑组也报错。关系路径排序**不等于**对象点号路径排序，不受 U4 影响。
8. **`timestamps` 校验**：仅 `true` / `false` / `"ms"` / `"s"`（缺省毫秒）；非法值注册即报错。单位换算由宿主时钟负责。
9. **联邦 `degraded` 不阻断**：无法下推的跨源分页/排序产出结构化事件，宿主应接入自动反馈闭环。
10. **不可翻译必须显式**（项目铁律）：翻译遇到无法安全处理的组合必须报错或产出 `unsupported` + warning，绝不产出缺段的 SQL。
11. **`planFederated` 的 `results` 必须与 `plan.sources` 同序同长**，否则 `mergeFederated` 会错位。

## FAQ

**rust-store 是 ORM 吗？**
不是。它是规划与翻译引擎。它不持有驱动、不打开连接、不执行任何东西 —— 每条命令都由宿主执行。

**使用数据层需要本仓库吗？**
不需要。安装 `nodejs-store`（npm）或 `storepy`（PyPI）即可。当你需要构建、调试或扩展引擎本身，或为另一种语言写宿主时，本仓库才重要。

**怎么写 GQL 查询？**
见 [GQL 能力](#gql-能力)。完整语法与示例也记录在配套的 `text-to-query` skill 中，它把自然语言问题翻译成 GQL + params。

**为什么 Node.js 与 Python 行为完全一致？**
两个绑定都包同一个 Rust core，只做 JSON 转换。宿主里没有重复逻辑，所以语义不可能漂移。对拍套件与黄金基准的存在，就是为了持续证明这一点。

**跨四个不同的数据库，聚合是怎么工作的？**
根级 `$group` / `$having` 映射到 `GROUP BY` / `HAVING`；关系聚合谓词映射到 `EXISTS` / `NOT EXISTS`；关系滚动 `agg` 计算列映射到派生表 `LEFT JOIN`。MongoDB 走原生管道。四个后端共享同一套语义。

**某些东西无法翻译成 SQL 时会怎样？**
引擎显式报错，或产出 `unsupported` + warning 事件。它绝不产出悄悄漏掉一段的 SQL。MongoDB 仍能执行少数 SQL 无法执行的东西（例如 `$group.by` 中的 object 点号路径），所以这些情况只在 SQL 侧是错误。

**可以直接从 Rust 使用吗？**
可以 —— `rust-store-core` 就是一个普通 Rust 库（`publish = false`，按 path 依赖）。注册 schema、规划查询、翻译命令，再用你选择的驱动执行。

## 相关项目

- [`nodejs-store`](https://github.com/coenddt/nodejs-store) —— Node.js 宿主（npm `nodejs-store`）。
- [`py-store`](https://github.com/coenddt/py-store) —— Python 宿主（pip `storepy`）。
- `text-to-query` —— 配套技能：把自然语言问题编译为本引擎所需的 GQL + params。

## License

[MIT](LICENSE)
