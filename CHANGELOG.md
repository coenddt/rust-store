# Changelog

本仓库是 `py-store` / `nodejs-store` 共享的 Rust 核心引擎（`core` + `core-node` / `core-py`
双绑定）。引擎侧的能力与破坏性变更在此记录；宿主侧（Python / Node）的用户可见变更见各自仓库
`CHANGELOG.md`。

## 2.0.0 (2026-09-14)

### Breaking Changes

- **计算列 `lookup` 形态归一为 `agg` 算子（多后端归一化 P4）**：schema `computes`
  的 `lookup`（Mongo 专用）形态移除，改用归一 `agg` 白名单算子 `$count`/`$sum`/`$avg`/
  `$min`/`$max`：`{"$count": "orders"}`（关系整名计数）、`{"$sum": "orders.amount"}`
  （必须带单级「关系.字段」）。空集语义（§9.7）：`$count` → `0`；`$sum/$avg/$min/$max`
  → `null`。执行按后端下推：SQL 走派生表 `LEFT JOIN (… GROUP BY fk)`，Mongo 走
  `$lookup` + `$addFields`。
- **删除用户 `$pipeline` 直通与 `plan_aggregate`（对齐 D3 / D18）**：GQL 中的
  `$pipeline` 参数**显式报错**（不再静默忽略）；`core` 的 `plan_aggregate`、
  双绑定 `core-node` / `core-py` 的 `planAggregate` / `plan_aggregate` 与
  `setAllowUserPipeline` / `set_allow_user_pipeline` 一并移除。归一聚合（`$group`/`$having`）
  已按统一 GQL 语法（固定阶段序，下推优先 + 内存兜底）在本批次回归（见 New Features）。
- **读路径关系权限收口（R0-1）**：GQL 显式请求的关系，若 `relation.read` 不可读、
  或目标 model 的 `schema.read` 不可读，规划期**直接返回** `Err(ERR_PERMISSION)`
  （错误码稳定前缀 `ERR_PERMISSION:`，Host 映射为 403）。此前是「静默裁剪该关系字段、
  查询照样成功」，现在改为直接失败。`ctx = None`（未设置上下文）维持 fail-open 放行，未变。
- **SQL 后端 `object` / `array` 字段不再静默丢弃**：
  - 读：显式投影 schema 声明为 `object` / `array` 的字段（SQL 侧无对应列）→ **显式报错**；
  - 写：`$set` / `$inc` 目标是 `object` / `array` 字段 → **显式报错**；`$unset` 维持跳过语义。
- **写路径静默点收口（§11.4）**：模型关系不可读时的写入数据由「静默丢弃」改为产出结构化
  `degraded` 声明（`plan.degraded` 数组，code `relationSkipped`）；未知 `rel_type`
  （非 `one` / `many`）由静默跳过改为 `Err`（D2：绝不静默）。

### New Features

- **归一化 P5：根级聚合与关系聚合谓词**：
  - 根级 `$group` / `$having` 成组聚合（计算列 `agg` 形态；固定执行序
    `WHERE → GROUP BY → HAVING → ORDER BY → LIMIT`，§9.3）；
  - §9.6 关系聚合谓词：`$condition` 中以**关系名**作键的 semi/anti-join 谓词
    （主形式 `filter` / `agg` / `having`；简写 `$exists` / `$count` / `$sum`… + `$of`），
    Mongo 侧归一为 `$lookup` + 哨兵代理键，SQL 侧翻 `EXISTS` / `NOT EXISTS`（§10.5 下推优先）。
- **写路径调用级 `now`（§11.3）与降级通道（§11.4）**：单次 `mutation` 调用内所有
  时间戳取同一 `now`，保证批次内确定性；`degraded` 经宿主 `feedback.emit` 告警。

### Bug Fixes

- **SQL 布尔列归一（§9.7）**：`dialect` 行还原时按 **schema 声明类型**（`boolean` / `bool`）
  把 MySQL `TINYINT(1)` / SQLite `INTEGER` 的 `1`/`0` 归一为 JSON `true`/`false`
  （PG 原生 `BOOLEAN` 为 no-op），与 Mongo 布尔语义对齐（`RowCol.is_bool`）。
- **PostgreSQL 浮点字面量参数类型**：PG 扩展协议由服务端按上下文推断 `$n` 类型，整数列旁的
  浮点值被推断为 `integer` 而报 `invalid input syntax for type integer`；`dialect` 现对
  非整数字面量显式标注 `CAST($n AS double precision)`（`binop` 与 `$in`/`$nin` 共用）。

### Migration

- 计算列原 `lookup` 声明改写为 `agg`（见上）；结果语义不变，且 SQL 后端自此同语法可用。
- 原先依赖 `planAggregate` / `store.aggregate()` / `setAllowUserPipeline` 的调用方：
  改用归一 `$group` / `$having` GQL 语法。
- 显式请求不可读关系现在会直接返回 `ERR_PERMISSION`（Host 侧 403）；不再假设
  「查询成功 + 关系被裁剪」。
- `object` / `array` 字段在 SQL 后端仍**不支持读写**（DDL 不建列），失败方式由「静默」
  变为「显式报错」，跨后端代码按「可能报错」处理。
