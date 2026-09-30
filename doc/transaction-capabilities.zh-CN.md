# 核心（Rust）侧的事务型能力（关系谓词 / 自增主键 / 分组路径）

> 面向事务型业务的能力增补在引擎层的说明（由 py-store / nodejs-store 两个宿主共享）。
> 宿主侧 API 与示例见各自仓库的 `doc/transaction-capabilities.md`。
> 变更留痕：common-store 仓库《事务型能力增补执行文档.md》。

## 1. 关系谓词：语法糖 + mutation 归一（`core/src`）

- **整值条件对象糖**（`pipeline/relation_filter.rs::parse_simple`）：谓词 spec 的键全不带
  `$` 前缀、也不是主形式键（`filter`/`agg`/`having`）时，视为 `$filter: {…}` + `$exists: true`
  （semi-join）。含任意 `$` 键的 spec 走原解析（既有 J 组负例文案逐字节不变）。
- **mutation 归一**（`command/mutate/mod.rs::plan_rel_pred_mutation`）：`updateMany` /
  `remove` 的条件引用关系名时，规划产出 `plan.lookups + {$match: <改写条件>} + {$project: {_id: 1}}`
  作为 **preCommand**，主命令条件改写为 `_id $in`（占位 `__REL_PRED_IDS__` 由宿主回填）。
  主命令刻意剔除代理键——写路径的 `build_filter` 没有关系解析器。
  - SQL：preCommand → `EXISTS`（既有 §10.5 下推），主命令 → 标量 `_id $in`；
  - MongoDB：两段原生命令直接执行——修复此前 Mongo 侧的**静默 no-op**
    （`modifiedCount=0` 无告警）；
  - 读权限校验（R6 关系级 / F3 子字段）由 `relation_filter::plan` 执行，与查询路径同源。
- `needs_new_id`（自增）：声明 `_id` 为 `strategy: "autoincrement"` 的 schema 恒不消耗
  宿主供给的 ID（`command/mutate/mod.rs`）。

## 2. 自增主键

- `build_insert_doc`（`command/write.rs`）：autoincrement 且无 `_id` 时绕过「未配置 idPrefix」
  报错、不注入 `_id`——SQL INSERT 的列白名单来自文档键，数据库自增列赋值；
- `dialect/write/insert.rs`：文档无 `_id` 列且后端支持 RETURNING 时，INSERT 追加
  `RETURNING _id`（PG/SQLite）；MySQL 由宿主 last-insert-id 回读；
- `computes/defaults.rs`：类型级隐式默认（`int → 0`）**不**作用于缺失的 autoincrement
  `_id`——零值会把「数据库待赋值」伪装成「已赋值」；
- 归档派生（`schema/registry.rs::archive_defn`）剔除 `_id.strategy`（归档显式拷贝源行 ID；
  MySQL 要求 AUTO_INCREMENT 列必须被索引）。

## 3. `$group by` one 关系路径

- `pipeline/group.rs::validate_by_key` 放行 `关系.标量字段` 路径（关系须为 `one` 且叶子是
  目标 schema 的标量字段）；**many 路径显式报错**（扇出破坏 `$count:*` 语义）；
- Mongo 规划（`build_stages`，签名新增 `schema` + `registry`）在 `$match` 之前发射
  `$lookup` + `$unwind {preserveNullAndEmptyArrays: true}`；
- SQL 翻译（`dialect/select/group_agg.rs::translate_group`，签名新增 `registry`）产出
  `LEFT JOIN g_<rel> ON g_<rel>.fk = t.local` 并按 `g_<rel>.<col>` 分组；空匹配归入 NULL 组，
  与 Mongo preserve 语义对齐；
- 双宿主保留各自的 DDL 生成器（输出逐字节一致），含本批一并加入的
  `CREATE [UNIQUE] INDEX` 生成。

## 4. parity 守护

- `cargo test -p rust-store-core`（158 个测试）覆盖本轮触及的 §9.6 改写、group 校验与
  dialect 翻译；
- 场景 e2e（`manager-transaction`，9 用例 × 4 后端 × 双宿主）+ `course-platform` 回归套件
  （101 用例 × 4 后端 × 双宿主）端到端守护；报告在各宿主 `doc/test-eval/`。

## 5. 规划中

声明式 schema 迁移（白名单：加表 / 加列 / 类型放宽 / 加索引；白名单外显式
`MIGRATION_UNSUPPORTED`）设计已完成，将落在**宿主**的 DDL 模块（schema→DDL 本就在宿主），
Rust core 保持无迁移代码。
