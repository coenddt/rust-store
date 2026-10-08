# Changelog

本仓库是 `py-store` / `nodejs-store` 共享的 Rust 核心引擎（`core` + `core-node` / `core-py`
双绑定）。引擎侧的能力与破坏性变更在此记录；宿主侧（Python / Node）的用户可见变更见各自仓库
`CHANGELOG.md`。

## 4.2.0 (2026-10-08)

### Added（触发器）

- **schema 声明式触发链**：schema 顶层 `triggers` 声明写事件（首批 `insert` / `update`）的副作用
  步骤，规划期展开为 `plan.triggers`——命令式 `{name, onFields, when, command}` 或回调式
  `{name, onFields, when, callback:{fnRef,args}}`，由 Host 在源写同一原子包络内执行。
  判定语义：update 事件先判 `onFields 值真的变化`（no-op 抑制）→ 再判 `when`。
- **注册期校验（零静默）**：事件键白名单（`remove` ⇒ Err）；命令式 `op` 白名单 `insert` / `update`
  （`upsert` ⇒ Err）；`cascade` 键拒绝（首批不支持级联）；`onFields` 必须为 schema 已声明字段；
  `when` 必须为对象。text2query 档下声明触发器即 `Err`（纯查询宿主禁写路径能力）。
- **触发器探针**：update 触发器把 `onFields ∪ {{before.*}}` 引用并入探针投影，Host 据探针结果做
  字段级命中判定并替换 before 占位符。
- **golden 守护**：`core/tests/triggers.rs`（含 `triggers_fixture_shape`）+ `fixtures/triggers/cases.json`；
  node / py 宿主对同一 fixture 的展开输出逐字节一致（`parity-triggers` / `parity_triggers` 对拍脚本）。

## 4.0.0 (2026-10-04)

### Breaking（定义零落点 + `namespace` 删名）

- **落点外置**：定义文件不再携带落点字段（`source` / `database` / `schema` 从定义中移除），
  定位语义中的 `namespace` 一词删除，改由「定义目录层级 + `store.config.json`」解析：
  定义根下一级 = `database`，PostgreSQL 再加一级 = `schema`，更深层级自由打平（无语义）。
- **命令体形状**：命令体定位字段改为 `{source, database, schema, collection}`；
  `routeOverride` 键由 `{source, namespace}` 更名为 `{source?, database?, schema?}`。
- **同批同名报错**：同一装载批次内出现重名定义 ⇒ 报错、服务不启动（原为静默覆盖）；
  同名语义 = 一份主（无 `replica`）+ 若干从（`{ "name": ..., "replica": true }`，仅声明链路，
  不进控制面）；主 0 份或多份 ⇒ 报错。

### Breaking（数据标识符下沉翻译 + 归一冲突）

- **数据标识符下沉翻译**：定义侧标识符（`collection` / 字段 / 关系字段 / 计算列键 / `fnRef` / 索引名）
  以任意风格书写，由引擎翻译为目标风格——SQL（MySQL / PostgreSQL / SQLite）落 `snake_case`，
  MongoDB 落 `camelCase`；宿主代码风格（Node / Java / C# / Rust `camelCase`、Go `PascalCase`、
  Python `snake_case`）随计算列。契约键（`fnRef` / `localField` / `foreignField` / `asyncFn` / `type` …）
  与 schema `name` 不翻译。
- **归一实现唯一**：归一化（canonicalization）唯一实现在 `core::naming`，经 `core-node` / `core-py`
  透出；宿主不得重复实现。
- **`ERR_NAME_CONFLICT`**：同 schema 内两个逻辑名归一后相等（如 `orderTotal` vs `order_total`），
  或某名归一后撞契约保留键（如 `fnref`）⇒ 报错、服务不启动（绝不静默覆盖）。
  **既有与保留键同名归一的数据标识符（如字段名 `type` / `read` / `write`）将被拒绝**。

### Breaking（计算列 `fnRef` 复合名 + 归一匹配）

- **默认复合名**：逻辑 `fnRef` 默认生成为 `<schema.name>.<计算列键>`（不再手写）；`name` 全局唯一
  ⇒ `fnRef` 全局唯一。跨 schema 复用实现时显式写共享名（如 `"fnRef": "common.moneyLabel"`），
  命名由必填降为可选。
- **归一匹配**：宿主实现按归一化绑定——实现名的宿主语言风格与 schema 逻辑 `fnRef` 均归一为 token
  序列比对（Node 的 `orderAmountLabel` 与 Python 的 `order_amount_label` 绑定同一逻辑计算列）。
- **`ERR_FN_MISSING`**：声明的 `fnRef` 必须有实现，否则服务启动失败。

### Migration

- 替换定义 / 命令中的 `namespace` → `database`（PG 再加 `schema`）；定义文件删除落点字段
  （`source` / `database` / `schema`）。
- 目录按新语义重排：一级 = `database`；PG 二级 = `schema`；更深层级自由打平。
- `routeOverride` 键改名为 `{source?, database?, schema?}`。
- 抬高绑定依赖下限：Node `^4.0.0` / Python `>=4.0.0,<5.0.0`。
- 文档面：`core` / `core-node` / `core-py` README 新增「命名规范与翻译摘要」
  （`SPEC:NAMING-STYLE` / `SPEC:FNREF` 定稿块）。

### 证据

引擎侧三 README 规范块一致性由 `tools/check-spec-snippets.js` 守卫（归一后逐段相等，缺失 / 不等即退出码 1）。

## 3.0.0 (2026-10-02)

### Breaking（RBAC 内置角色清单化，设计 §11）

- `super_admin`/`admin` 不再默认放行：豁免改为 `set_exempt_roles` 清单（默认空）；
- `guest` 不再默认拒写：拒写改为 `set_deny_write_roles` 清单（默认空）；
- `guest` 无白名单读拒在默认 Open 姿态下不再保留：未配置姿态显式化为
  `set_unconfigured_policy`（默认 `open`；`closed` = 未配置模型读写全拒）；
- RBAC enforce 模式升级为无例外 default deny（豁免清单为空时无后门）；
- 新增绑定/宿主透传：core-py/core-node/core-ffi/host/py-store/nodejs-store/go-store
  各 3 个配置方法；creator 伪角色、internal、策略 JSON、错误前缀契约不变。

## 2.7.0 (2026-10-02)

### Added

- **RBAC 动态策略（三绑定透传）**：`core` 判决 + `core-node` / `core-py` / `core-ffi`
  导出 `setRbac`（注入/清除，解析失败显式报错）与查询面 `rbacCan` / `rbacReadableFields` /
  `rbacWritableFields` / `rbacRowCondition`；plan 链路拦截自动生效，host 层 `Store::set_rbac`
  同步注入。
- **`readableComputes` 角色判决导出**（core-node napi + core-py pyo3）：宿主描述面
  （describeForAi / describe_for_ai）可按角色收放计算列；`clearSchemas` 一并导出。
- **`ERR_TEXT2QUERY` 档位哨兵前缀**：text2query 档 U1~U4 等收缩判决携带稳定前缀，
  宿主可据此把 `planError` 归类为 `profileBlocked`（nodejs-store ask 回喂已接入）。

### 证据

workspace 全量测试回归全绿（含 `core/tests/guards.rs` / `profile.rs` 新增的前缀断言）。

## 2.6.0 (2026-09-30)

### Added

- **mutation 关系谓词归一**（`command/mutate`）：条件含关系名时经 `relation_filter::plan`
  （含 R6/F3 读权限）产 preCommand（aggregate：`$lookup` 挂数组 + `$match` 代理键 + `$_id`
  投影），主命令条件改写为 `_id $in`（占位由宿主回填）；`parse_simple` 新增整值条件对象糖
  （无 `$` 键 ≡ `$filter` + `$exists: true`；含 `$` 键走原解析，J 组负例文案不变）。
- **自增主键契约**：`FieldDef.strategy`（仅 `autoincrement`，注册期校验）；`needs_new_id`
  对 autoincrement 恒 false；`build_insert_doc` 放行无 `_id` 文档；SQL INSERT 自动追加
  `RETURNING _id`（支持 RETURNING 的后端）；`apply_defaults_and_computes` 对 autoincrement
  `_id` 跳过类型默认值（`int→0` 会伪装「已赋值」）；归档派生剔除该策略。
- **`$group by` one 关系路径**：`validate_by_key` 放行 one 关系路径（many 显式 Err）；
  Mongo 侧 `build_stages` 发射 `$lookup`+`$unwind(preserve)`；SQL 侧 `translate_group`
  生成 `LEFT JOIN g_<rel>` 聚合（空匹配归 NULL 组，语义对齐）。

### 证据

core 单测 158/158；场景 e2e（`manager-transaction` 9 × 4 × 双宿主）与 course-platform 回归
（101 × 4 × 双宿主）全绿。宿主侧用户可见变更见各自仓库 CHANGELOG。

## 2.5.0 (2026-09-29)

### New Features

- **原生 SQL 语句编译器（`raw_stmt_compile`，`dialect/raw.rs`）**：为宿主 `execute_raw` /
  `executeRaw` 提供 core 侧唯一实现——位置档（params 为数组/null：SQL 原样透传，对标 SQLAlchemy
  `exec_driver_sql()`）与命名档（params 为对象：`:name` 按出现顺序编译为方言占位符、参数按引用
  顺序重排、同名复用，跳过 `::` cast / 引号 / 注释边界，对标 SQLAlchemy `text()`）两档，附读写
  推断；`core-py` / `core-node` 双绑定透传。

## 2.3.0 (2026-09-27)

### New Features

- **调用档位（`Profile`）：`standard`（默认）/ `text2query`，判决唯一在 core**：`Registry` 新增
  `profile` + `set_profile` / `profile`（未知值 **Err**，禁静默回落）；`core-py` / `core-node`
  双绑定透传（蛇形 / 驼峰）。
- **`text2query` 档硬限制**（相对 `standard` 全为收紧）：单次取数行数封顶 `T2Q_MAX_ROWS = 1000`
  （根级省略即视为上限、关系级仅夹上限）、关系嵌套深度 `T2Q_MAX_DEPTH = 3` 超限 **Err**、
  联邦单源行数 `T2Q_MAX_FEDERATION_ROWS = 10_000` 超限 **Err**、`route_override` 非空即 **Err**
  （CWE-639）、强制携带用户上下文；DB 独有能力（`$pipeline` 直通、`$group.by` object 点号路径）
  一律 `forbid_t2q` 显式 **Err**（功能收缩，前缀 `ERR_TEXT2QUERY:`）。
- **`standard` 档跨 DB 对齐后放开**：`object` / `array` 字段落 **JSON 列**
  （MySQL `JSON` / PG `jsonb` / SQLite `TEXT`+JSON1）后，**U1~U4**（数组/对象整值条件、object
  点号路径过滤与排序）四库均可下推（U2 对象键序差异按后端**告警**）；根级 `$pipeline` 直通
  （仅 Mongo 源；SQL 侧阶段可翻译时下推、含 Mongo 独有阶段显式 Err）；`$group.by` object
  点号路径（仅 Mongo 源）。DB 独有能力处均标注「⚠️ 不建议业务查询（迁移 / 脚本用）」注释块。
- **关系聚合谓词（§9.6）子级 `filter` 扩展**：U1~U3 按档（`standard` 放行 / `text2query` **Err**）；
  **一层嵌套关系下钻**（如 `{"lessons":{"$filter":{"children.seq":{"$gt":1}}}}` → SQL 嵌套 `EXISTS`；
  三级路径与 `$or` / `$nor` 内嵌套 → **Err**，禁静默近似）。

### Breaking Changes

- **关系嵌套超深由静默降级改为显式 Err**：此前超 `MAX_DEPTH` / 分页深度**静默降级为空 `$lookup`**
  （返回残缺数据却报成功），现两档均 **Err**（依据「禁静默失守」）。
- **`$pipeline` 由全局拒绝改为按档分流**：`standard` 档放行（Mongo 源可执行；SQL 源逐阶段翻译，
  无法映射即 `PushdownUnsupportedError`），`text2query` 档维持 `ERR_TEXT2QUERY:` 拒绝；
  `$out` / `$merge` 写副作用阶段两档均拒。
- **联邦单源行数超限报错**：`merge_federated` 由「超限仍全量拉取」改为显式 **Err**
  （拒绝静默全表拉取），上限随 plan 下传（按档取值）。

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
