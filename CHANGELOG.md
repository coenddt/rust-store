# Changelog

本仓库是 `py-store` / `nodejs-store` 共享的 Rust 核心引擎（`core` + `core-node` / `core-py`
双绑定）。引擎侧的能力与破坏性变更在此记录；宿主侧（Python / Node）的用户可见变更见各自仓库
`CHANGELOG.md`。

## 6.0.3 (2026-11-14)

### Fixed

- **PyPI 发布流水线加固 + 补齐 `6.0.2` 的 cp310 平台缺口**。`release-pypi.yml`
  此前用 maturin `--find-interpreter`「发现多少出多少」，构建环境（maturin-action
  容器镜像 / runner 自带解释器）漂移时产物跟着漂移且全 job 依然 success。
  修复：解释器集合改为由 workflow 显式固定——manylinux（钉 `2014` 镜像）与
  musllinux 容器内显式 `-i python3.10..3.13`；Windows / macOS 改为解释器 × 目标
  矩阵逐个构建；每个构建 job 增加产物 wheel 标签校验 step、publish 前再校验
  产物总数（24 = manylinux×2 + musllinux + win + macos×2，各 ×4 解释器），
  缺哪个直接红，发布侧也过不了。另修两处首发过程中暴露的 workflow 缺陷：
  ① macos 矩阵的 include 条目不含 python 键时 GitHub 丢弃该组合（macos x86_64
  四个 job 未生成），拆为两个显式 job；② 产物校验不能按整段子串匹配 wheel 名
  （pytag 与 abitag 重复出现，如 `-cp312-cp312-win_amd64`），改为按下标解析。
  发布入口新增手动 dispatch（`publish: true`）用于流水线自身修复后补发当前版本。
  覆盖 CPython 3.10–3.13 ×（manylinux x86_64/aarch64 + musllinux x86_64 +
  win_amd64 + macos x86_64/arm64）；free-threaded（cp314t+）与 PyPy 暂不覆盖
  （`6.0.2` 的 cp314/cp315/PyPy wheel 是当时构建环境解释器集合的副产物，非承诺
  范围），宿主侧有需要再扩。

## 6.0.2 (2026-11-14)

> **PyPI 侧缺 cp310 的 win / macOS wheel**（发布日复核修正：此前误记为
> 「只剩 cp310/cp311 缺 cp312」——实际 cp311–cp315 与 PyPy 齐全，缺口是
> `cp310-win_amd64`、`cp310-macosx_10_12_x86_64`、`cp310-macosx_11_0_arm64`
> 三个 wheel，见 6.0.3；目标镜像 python:3.12.8 的 cp312 wheel 不受影响）。
> crate / npm 侧 6.0.2 完好无损。

### Fixed

- **asyncFn 计算列按请求字段执行**（与 `release/5.x` 线 `5.3.4` 同源修复）：`select_async_fns` /
  `run_async_fns` 新增 `requested`（查询显式请求的字段快照）参数，只执行、只写入被显式请求的
  asyncFn 计算列。修复前任何查询都执行模型全部 asyncFn——IO 型批量计算列（如「逐单统计明细数」
  ~90ms/次）在**未请求它的查询**（存在性检查、同模型其他投影）上被强加白付开销（实测该类查询
  单次 ~110ms → 修复后 ~20ms）；且写入发生在 `process_node` 裁剪之后、无人剥离，未请求的
  计算列值会泄漏进返回结果，破坏 GQL 投影语义。显式请求的计算列照常执行（合法开销不受影响）。
  `depends` 注入字段的 strip 机制不受影响。
  对拍（parity）用例中两处 gql 输入显式化以保持旧快照语义（`pp-async-strip` / `pp-sync-and-async`），
  `run_async_fns` 直连场景由调用方传入 requested；新增回归测试 `core/tests/async_fn_projection.rs`。
- **绑定层补齐 asyncFn 选取入口**（`core-py` / `core-node`，随上条同步，无破坏性变更）：
  - `async_fn_refs(model, ctx?, fields?)` 追加**可选** `fields`（追加在参数表末尾，旧宿主位置传参
    不受影响）：缺省仍列出全部 `fnRef`（与前版本行为一致），给了则与 `prepare_query` 的 `fnRefs`
    同口径，只返回被显式请求的计算列。
  - 新增 `all_async_fn_keys(model, ctx?)`，返回全部 asyncFn 计算列的键（仅按 `comp.read` 裁剪），
    作为「列出全部」的显式入口。
  - 新增绑定契约测试 `core-py/test/async_fn_refs_bindings_test.py`、
    `core-node/test/async_fn_refs.bindings.test.js`。
- **CI 门禁补齐测试覆盖面**：`core-node` 的 `npm test` 补登 `test/async_fn_refs.bindings.test.js`
  （此前为新文件但不在硬编码清单里，等于未执行）；`py-binding` job 由只跑 `parity_test.py`
  改为跑整个 `core-py/test/` 目录，避免新增测试文件静默漏跑。
- **三端版本统一为 `6.0.2`**（crate / PyPI / npm）。修复：5.x 线合入 main 时
  `core/Cargo.toml`（及根 `Cargo.lock`）的版本号被覆盖回 `5.3.1`，crates.io 上
  `5.3.1` 已发布，若不修正直接推 tag 会使 release-crates 因版本已存在而失败；
  本次随 6.0.2 一并收口。

## 6.0.1 (2026-11-10)

> 本版本是 `6.0.0` 的补丁批次；内部构建号 `+fix1` / `+fix2` 均对应发布版 **`6.0.1`**：
> `fix1` = 计算列 `depends` 自引用注入修复（BUGMW6VKGIVE050），`fix2` = 三个 upsert 缺陷修复。
> 三端（crate / PyPI / npm）统一为 `6.0.1`（`6.0.0` 已被 crates.io 占用，同号不可重发）。

### Fixed

- **计算列 `depends` 含计算列自身键时跳过注入**（BUGMW6VKGIVE050）：`collect_field_deps`
  遇到 `depends` 引用自身键不再把该键注入，避免计算列依赖自身造成循环。
- **upsert `$setOnInsert` 覆盖 filter 等值键**（P0）：`merge_insert_defaults` 改为接收执行条件，
  filter 中直接等值 `{field: v}` 与显式 `{field: {"$eq": v}}` 的顶层键不再补 schema default
  （`$and` 内等值键同样跳过；`$in` / `$ne` / 范围算子仍按非单值基线处理，default 正常写入）。
  修复前 `userId` / `auctionItemId` / `moduleType` / `resourceId` 等基线被写成空串，
  导致 upsert 幂等失效并静默绕过 partial unique 索引。
- **upsert 与原生操作符 update 未校验 object 字段**：`plan_upsert` 与 `build_raw_update`
  补调 `validate_object_fields`，与 insert / 普通 update 同码拒绝写入非对象值（internal 上下文不豁免）。
- **`plan_upsert` 把 `_id.default` 注入 `$setOnInsert`**：构造 `$setOnInsert` 时无条件排除 `_id`，
  避免 upsert 命中已有文档的更新分支触发 MongoDB `ImmutableField`；显式传入的 `_id` 不受影响。

## 6.0.0 (2026-11-08)

> 相对已发布的 `v5.3.2`。破坏性项集中在权限与默认值语义。先看迁移清单，再看逐条明细。

### 迁移清单

| 若你在…… | 升级后的行为 | 迁移动作 |
|---|---|---|
| schema 里写了 `ownerKey: "_id"` | 注册期显式报错（此前静默把 `_id` 当归属字段） | 改用独立字符串字段（如 `createdBy`）；`ownerKey` 字段类型必须是 `string` |
| 客户端 insert 时传 `createdBy` | 被忽略并由 `ctx.userId` 覆盖；取不到 userId 报 `ERR_NO_WRITE` | 归属键交系统维护；仅 `ctx.internal` 的系统调用可显式写入 |
| 客户端 update 的 `$set` / `$unset` 触及归属键 | 报 `ERR_PERMISSION`（含 `createdBy.x` 点号路径与 `$set` 内操作符） | 从写载荷中移除归属键 |
| `write` 含 `creator` 的 update / remove | 先返回 `{needsProbe}`，宿主回喂命中文档后二次规划 | 宿主实现 probe 重入；`updateMany` / `removeMany` 无 userId 报 `ERR_NO_BATCH_WRITE` |
| 依赖 `count` 在无读权限时仍返回行数 | 改为 `ERR_PERMISSION`（与 query / exists 同码） | 给调用方补读权限 |
| 用 `read: []` 表达「谁都不许读」 | 恢复 5.x 旧义：空名单 = 未配置 = 不限制 | 改为 `denyRead: true` |
| 依赖 update 的 `$set: { a: null }` 把字段写成 null | 该字段不更新（与 5.x 旧义一致） | 清空字段改用 `$unset: ["a"]` |
| Node 宿主读取超出 IEEE-754 安全整数范围的值 | 出参由 `number` 改为驱动原始字符串（宿主 6.0.0 同版本线，`toNumber` 超界） | 按字符串处理，不做数值运算或比较 |

### Breaking（权限收紧：归属键与 creator 归属）

- **`ownerKey` 不得为 `_id`**：注册期显式拒绝（此前静默把 `_id` 当归属字段，探针与注入全部失效）；
  声明的 `ownerKey` 字段类型必须为 `string`，否则注册报错。
- **归属键只由系统维护**：`insert` 忽略并覆盖客户端传入的 `createdBy`（取 `ctx.userId`，
  取不到则 `ERR_NO_WRITE`）；`update` / `updateMany` 的 `$set` / `$unset` 若触及归属键
  （含 `createdBy.x` 点号路径与 `$set` 内操作符）直接报 `ERR_PERMISSION`。
  **例外：`ctx.internal`（宿主可信系统调用：metadef 落库 / 迁移 / 种子数据）允许显式写入
  归属值**——不剔除、不覆盖，也不再因缺少 `userId` 报错（此前 internal 同样被剔除，
  导致宿主 `test_metadef` 的 `KeyError: 'createdBy'`）。
- **creator 受限写入先探针**：`write: ["creator"]` 的 `update` / `remove` 即使 ctx 有 userId，
  也先返回 `{needsProbe}`（宿主回喂命中文档后二次规划），执行条件强制并入归属；
  `updateMany` / `removeMany` 无 userId 报 `ERR_NO_BATCH_WRITE`，单条 `upsert` 无 userId 报 `ERR_NO_WRITE`。
- **RBAC `ownerOnly` 的 remove/update**：探针命中后执行条件改为「原条件 ∧ 归属」，
  不再以原始条件下发 `deleteMany` / `updateMany`。

### Changed（空名单保持 5.x 旧义 + 新增显式拒绝开关）

- **`read: []` / `write: []` / 字段级 `read: []` 维持 5.x 旧义：空名单 = 未配置（不限制）**。
  本轮曾拟改为「显式空名单 = 拒绝」，因会破坏现有下游（不改代码/配置即失效）而回退；
  语义收口回 `permission::evaluate`，空名单与键缺失同走 `unconfigured` 姿态（默认 `Open`）。
- **新增顶层 `denyRead`（可选布尔，缺省 `false`）**：`denyRead: true` 才表达「谁都不许读」。
  - 判据**单点**在 `permission::can_read_schema`（query / count / exists / lookup / federation
    全部读路径共用），优先于一切角色（含 `internal` 与豁免清单）；
  - `ctx = None`（系统内部调用，fail-open 契约）不受影响，需要连内部调用一并拒绝时配合
    `require_context(true)` 使用显式系统上下文；
  - `read: []` 与 `denyRead: true` 并存 ⇒ **`denyRead` 生效**（显式拒绝意图优先）；
  - 注册期做类型校验：非布尔报错（`denyRead 必须为布尔值`）；未知顶层键仍照旧报错。
  - 与宿主 workflow 白名单同一口径：**空名单不构成拒绝，拒绝必须显式表达**（宿主对齐结论
    见 `common-store/doc/topic/自检-store核心与绑定-第二轮/版本归类与分支策略.md` §九）。
- **不变**：`None`（未配置）按 `unconfigured` 策略；`internal`、豁免角色清单、`ctx=None`
  的直通语义不变。

### Added

- **`Registry::unregister`（schema 注销 / 原子回滚原语）**：`register_batch` 单批次失败本就零副作用，
  但宿主 `start()` 若逐条 `register`，中途失败会残留已注册项。新增
  `Registry::unregister(name) -> Result<(), String>`：
  - **未知 name 幂等成功**（回滚清单可整份重放；注册期错误仍原样上抛，不吞错）；
  - 注册期自动派生的 `<Name>Deleted` 归档表一并清理（按 `Entry::archive_of` 精确匹配，
    不按名字后缀猜，独立注册的同名归档不受影响）；
  - `indexes` / `field_order` / 链路 / 版本均内嵌于被删 `Entry`，无全局派生缓存；
  - 版本号不保留（同名重注册从 1 重新计数，其他 schema 不受影响），不引入墓碑表；
  - 非 base 策略视图调用 ⇒ `ERR_POLICY_VIEW_READONLY`。
  - 绑定：`core-py::Registry.unregister(name)`、`core-node::Registry.unregister(name)`、
    `host::Store.unregister(schema_name)`。
- **`plan_exists_with_ctx`（带 ctx 的存在性判据）**：`exists` 此前只走无 ctx 的 `plan_exists`，
  宿主公开 `exists()` 因此可绕过 `read: []`、读白名单与 RBAC 探测任意条件是否命中。
  新入口与 `plan_count` 复用同一套读校验（`ensure_context` → `can_read_schema` →
  `rbac::ensure_read`），并注入静态 owner 条件与 RBAC 行条件；返回结构与旧入口一致
  （`findOne` + `{_id:1}`）。旧 `plan_exists` 保留为无 ctx 的内部入口（语义不变）。
  绑定：`core-py::Registry.plan_exists_with_ctx(model, condition, ctx, route_override)`、
  `core-node::Registry.planExistsWithCtx(model, condition, ctx, routeOverride)`。
- **host `schema_definition`（schema 只读定义导出）**：协议皮原先只能拿到 `schema_fields`
  的字段名列表，无法判断点路径是否声明/类型；新增
  `Store::schema_definition(schema_name) -> Result<Value, String>`，按 `field_order` 输出
  core `FieldDef` 直出（`type` / `required` / `default` / `read` / `write` / 嵌套 `fields`
  原始定义 / `strategy` / `dim` / `metric`）。host 只暴露信息，白名单与类型判定语义留在协议皮。
- **`apply_write_computes`（写后仅叠同步计算列）**：`update` 的 `returns` 此前没有 core 出口，
  宿主只能按「defaults 键序 + 落库 doc 基」自行兜底拼装，计算列缺失。新增
  `computes::apply_computes` 的绑定导出：`core-node::Registry.applyWriteComputes(model, doc)`、
  `core-py::Registry.apply_write_computes(model, doc)`、`host::Store.apply_write_computes(schema_name, doc)`。
  - 入参必须是 `returnDocument: AFTER` 的**落库文档**；只叠 `computes` 中的同步 `fn`
    计算列，**不补任何默认值**（D1：默认值只在 insert 物化一次，重复填充会让
    「落库 / returns / 读回」出现三种取值）；
  - 无 `fn` 声明原样返回；声明 `fn: true` 却未注册实现时显式报错，不静默跳过。

### Fixed（语义一致性）

- **insert 返回 == 落库文档**：`returns` 只补 computes，不再补字段类型零值（此前返回 `views: 0` /
  `secret: ""` 而库里无该键）；仅显式 `default` 参与落库。
- **显式 `null` 可读回**：默认值回填只在键**缺失**时生效，`null` 不再被默认值覆盖。
- **嵌套 object 与数组的默认值规则统一**：object 字段值必须是对象（否则报
  `字段 {key} 声明为 object，实际值不是对象`），object 内部默认值按缺失键递归回填。
- **未声明关系报错**：聚合 `$lookup` 引用未在 schema 声明的关系时报错，不再静默丢弃。
- **upsert `$setOnInsert` 合并字段 `default`**：与 `insert` 对齐，显式传入值优先。
- **SQL 列序固定**：`writable_cols` / `writable_cols_union` 按 schema 声明序（新增
  `Schema.field_order`）发射，不再随 `HashMap` 迭代随机。
- **归档模型继承归属字段**：自动补出的 `ownerKey` 字段随归档 schema 一起下发。
- **注册规范化**：顶层未知键、字段定义未知键（递归）、`indexes` 非法结构 / 引用不存在的键
  均注册报错，不再静默丢弃。允许键清单以宿主真实形状为准：顶层含 `system`（正式可选布尔键，
  缺省 `false`，非布尔报错；落到 `Schema.system`，仅作「宿主内建模型」分类标记，归档派生继承，
  **不影响 DDL 与权限判定**）、`denyRead`（可选布尔，缺省 `false`，非布尔报错；显式拒绝读，
  判据单点在 `can_read_schema`）、`description`，字段级含 `description`，并允许 `x-*`
  前缀扩展键（`x-graphql` / `x-grpc` / `x-mcp` 等协议适配注记）。
- **FFI ctx 往返**：`accessKeys` 清单键完整往返；`internal` 与 py/node/host 共用
  `context_from_value`（`is_truthy` 语义），不再各自解析。
- **count 与 query 共用读判据**：`plan_count` 此前不做表级读校验，未命中读白名单的角色
  （含 `denyRead: true` 的模型）仍可 `countDocuments` 数出全表行数（RBAC enforce 的 default deny
  同样被绕过）；现与 query 一致走 `can_read_schema` + `rbac::ensure_read`。
- **显式 `null` 维持 5.x 旧义：不写该字段**：`remove_undefined` 与 dialect 的 `$set` 继续跳过
  `null`（`$set: {a: null}` = 不更新 a）；清空字段请用 `$unset`（物理列置 NULL 并从 `__present`
  移除令牌）。此前曾拟把 `null` 当「显式存在」落库，因会改变现有下游读回结果而回退。
- **upsert 显式 object 递归补默认**：`$setOnInsert` 的探测 probe 一律填 null，显式传入的嵌套 object
  无法补声明默认；现按真实值探测（同一路径仍只落 `$set`，避免 Mongo 同路径冲突）。
- **FFI 单测恢复执行**：`core-ffi` 的 `test = false` 使 C 边界单测从未运行；已开启并补
  `rcore_translate_name` 的 ABI 契约（正常往返 / 空指针 / 非法 UTF-8 / 未知 backend）。

### 宿主侧同版本线（node 6.0.0，不在本仓 core）

- **`toNumber` 超界出参**：`nodejs-store` 的 `fd597f9` 把超出 IEEE-754 安全整数范围的整数
  从 `number` 改为保留驱动原始字符串（`9223372036854775807` 不再变成 `9223372036854776000`）。
  读该列并做算术/数值比较的 Node 下游会失效。**本仓 core 无代码改动**，core 内
  `to_number`（分页 `page` / `pageSize` / `$skip` / `$limit` 归一）语义不变；此项随
  `rust-store-node` 6.0.0 同版本线一并告知。

## 5.3.3（维护分支，未发布）

> 维护分支只收**非破坏性**改动：纯新增符号 + 安全修复。现有 5.x 下游不改代码/配置即可升级。

### Added（纯新增，现有下游无需改动）

- **`Registry::unregister(name)`**：注销 schema（含注册期自动派生的 `<Name>Deleted` 归档表）。
  未知 name 幂等成功（宿主 `start()` 失败回滚清单可整份重放）；版本号随项删除，同名重注册从 1
  计数；策略视图调用返回 `ERR_POLICY_VIEW_READONLY`。经 core-py / core-node / host 同步导出。
- **`plan_exists_with_ctx(...)`**：带 ctx 的存在性判据，与 `plan_count` 同一套读校验
  （表级白名单 / RBAC / 归属与行条件），关闭宿主 `exists()` 的越权旁路；旧 `plan_exists` 保留
  为内部语义（`ctx = None`）。
- **`Store::schema_definition(name)`**（host）：只读导出字段类型与嵌套 `fields` 原始定义，
  供协议皮判定点路径声明与类型。5.x 无字段声明序跟踪，键序不保证跨进程稳定。
- **顶层 `denyRead`（可选布尔，缺省 `false`）**：`denyRead: true` 表达「谁都不许读」，
  与 `read: []` 解耦（`read: []` 维持 5.x 旧义 = 未配置 = 不限制）。判据单点在
  `permission::can_read_schema`，优先于一切角色（含 `internal` 与豁免清单）；`ctx = None`
  保持 fail-open（需连内部调用一并拒绝时配合 `require_context(true)`）。非布尔在注册期
  显式报错。与主干 `6.0.0` 同键名、同判据、同优先级。
- **`apply_write_computes`（写后仅叠同步计算列）**：收口 `update` 返回值缺计算列的兜底。
  只叠 `computes` 中的同步 `fn` 计算列、**不补默认值**（D1：默认值只在 insert 物化一次）；
  无 `fn` 声明原样返回，声明 `fn: true` 未注册实现时显式报错。经 core-py / core-node 导出
  （host 同名转发）。

### Fixed（安全收紧：count 读判据）

- **`plan_count` 补表级读判据**：与 `query` / `plan_exists_with_ctx` 同码
  （`can_read_schema` + `rbac::ensure_read`）。此前 5.x 的 count 不受读白名单 / `denyRead` /
  RBAC 约束，越权者可数出全表行数。**行为变化**：对无权调用者由「返回行数」变为
  `ERR_PERMISSION`（安全修复，升级说明需告知）。

### 已知语义差异（与主干 `6.0.0`，5.x 不改变）

- **写路径空名单口径不同**：5.x 的 `write: []` 表示**拒绝一切写**（非 `internal` 且不在
  `exemptRoles` 的调用者一律 `ERR_PERMISSION`，即使 `unconfigured.allow` 为真）；主干 `6.0.0`
  的 `write: []` 与键缺失同义，按 `unconfigured` 姿态判定（默认**不限制**）。
  - 5.x 用户若要「只允许特定角色写」，必须**显式列出写名单**（如 `write: ["admin"]`）；
    写 `write: []` 是「谁都不许写」的显式封锁，不要当成「未配置」。
  - 本差异**保留**：把 5.x 改成主干的空名单语义属破坏性变更（原本被拒绝的写会重新放行），
    违反维护分支只收加法与安全修复的原则。
  - 读路径两线已一致：`read: []` 均为「未配置 = 不限制」，要禁止读一律用 `denyRead: true`。

## 5.2.1 (2026-11-02)

### Fixed（发版 CI 修复，能力不变）

- **pyo3 `0.29.2` → `0.29.3`**：`pyo3-ffi` 0.29.2 的 `PyPy + Py_3_12` cfg 缺陷（`fn PyUnstable_InterpreterFrame_*`
  只判 `Py_3_12` 未排除 PyPy）导致 manylinux-aarch64 / musllinux 在构建 PyPy 3.12 wheel 时报
  `cannot find type _PyInterpreterFrame / c_int`；0.29.3 已补 `#[cfg(not(PyPy))]`。
- **版本声明点补齐**：npm 主包与 5 个平台子包、`core-py/pyproject.toml` 统一到 5.2.1
  （此前滞留 5.1.0，导致 npm `E403` 重复版本、PyPI 构建产物版本与 tag 不一致）。
- **clippy 门禁**：`access.rs` 抽 `ActiveWriteKey` 类型别名消除 `type_complexity`；
  测试中 `== false` 改为 `!` 消除 `bool_comparison`。

## 5.2.0 (2026-11-02)

### Added（feat：`accessKeys` 声明式行级权限）

- schema 新增 `accessKeys` 声明式行级权限：`read`/`write` 引用 ctx 清单，`bypassRoles` 豁免；
  单值字段成员判定、array 字段读交集 / 写子集；ctx 缺键 fail closed，空清单读空结果、写全拒。
- 方言：MySQL `JSON_OVERLAPS`、PG `&&`、SQLite `json_each`；Mongo 原生 `$in`。

## 5.1.0 (2026-10-26)

### Added（feat：`fields` ∩ `computes` 同名放行——计算列可覆盖同名字段）

- **命名冲突检测改为分组口径**（`schema/registry.rs::build_schema`）：
  `fields` / `relations` / `computes` 三者各自组内归一撞名仍报错；
  `fields` ∩ `relations`、`relations` ∩ `computes` 仍报错；
  **`fields` ∩ `computes` 放行** —— computed 值拥有同名键的输出所有权，
  `fields` 声明保留用于写入校验、DDL 与 `$inc` / `$set` 等原生增量更新，
  两者职责不同层，均不删除。字面保留键仅 `_score`（三命名空间仍拦截），
  `_id` 豁免逻辑不变。`detect_conflicts` 本身签名与行为未改，由 5 次分组调用实现
  「组内 + 指定跨组」拦截（`seen` 为调用内局部量）。
- **与 `nodejs-store` 对齐**：JS 宿主 schema 注册不做 fields/computes 同名检测，
  该组合在 JS 端一直可用；本次补齐 rust core（py-store / go-store 随之对齐），
  消除「同一 schema 在 JS 端可注册、Python 端注册即抛 `ERR_NAME_CONFLICT`」的缺口。
- **执行期覆盖顺序核实（无需改执行代码，新增测试固化）**：
  - `fn`：`process_node` 时序为 flatten → collect → fill_defaults → run_computes，
    computed 值直接 `insert` 覆盖同名键，位于字段默认值回填之后；
  - `asyncFn`：`prepare_query`（同步 fn）→ `run_async_fns` → `strip_query`，覆盖顺序正确；
  - `agg`（Mongo）：`$match` → `$lookup` → `$addFields` → … → 最终 `$project`，
    agg 键在投影阶段被重新加入（`agg_compute_keys`），覆盖发生在字段带入之后；
  - `agg`（local）：存储 7 → 输出计算值。
- **SQL 方言路径**：`translate_find` 跳过计算列（不为同名键重复下推物理列）；
  `translate_aggregate` 的 `root_selected` 排除 `compute_aggs` 键，派生表以
  `AS "<key>"` 单次输出计算列别名 ⇒ 不产生重复列名，也不返回 DB 旧值。
- **写入路径不变**：insert 只落 `fields` 值；`$inc` 原样透传、`$set` 仅做 RBAC 过滤，
  同名键按 `fields` 声明正常通过（已有测试固化，7 → 8）。
- 测试：新增 `core/tests/compute_override.rs`（12 例，覆盖 6 种命名组合与
  执行期覆盖顺序 / Mongo `$addFields` 位置 / SQL 列名唯一性）。

## 5.0.0 (2026-10-25)

### Breaking（撤销：用户 `$pipeline` 直通）

- **根级 `$pipeline` 直通恢复为「一律显式拒绝」**：撤销「双门禁分离」中
  `standard` 档放行 `$pipeline` 的裁定，恢复文档长期承诺的语义 —— GQL 中出现
  `$pipeline` 即在解析层（`pipeline/parse.rs::parse`）返回
  `ERR_GQL_PARSE:$pipeline 直通已移除（原生聚合请改用 executeNative / execute_raw）`。
  两档（`standard` / `text2query`）一致，不再按档位放行、不再静默忽略。
- 撤销理由（实测证据）：该路径的投影由 `build_pipeline_projection` 单独构建，
  只请求 `{ _id }` 时返回 `None` → 不下发 `$project` → **全字段返回**；且该函数
  从不调用 `apply_permission_prune` → **字段级读权限不裁剪**；同时
  `postprocess = None` → 计算列不参与。即「文档说已移除、代码里按档放行、且伴随
  两个静默绕过」，无法如实描述为契约。
- 连带移除（死代码 / 分支）：
  - `pipeline/build.rs`：`custom_pipeline_branch`、`override_or_append`（根参数
    「覆盖/追加」落位语义）、`$pipeline` 分支块；
  - `pipeline/projection.rs`：`build_pipeline_projection`（含上述两个绕过）；
  - `command/query.rs`：`has_pipeline` 及全部分支（依赖注入 / 展平 / 投影 /
    postprocess / find 快路径 / 两阶段优化判定）；
  - `federation/plan/mod.rs`：`has_pipeline` 用法与「联邦不支持 `$pipeline`」判定；
  - `pipeline/util.rs`：`find_stage_idx`。
- 行为影响：`nodejs-store` / `py-store` 共用本 core，两宿主同步变化；不存在
  「Mongo 源可用 / SQL 源报 unsupported」的分流。原生聚合逃生口仍由**显式入口**
  提供：`store.aggregate()`（Host 侧严格 opt-in）、`executeNative()` / `execute_raw()`。
- 前史：本条撤销宿主侧执行文档
  `py-store/doc/execution/2026/09/已完成-双门禁分离-执行.md` §4.1 #5
  「DB 独有能力可用」的裁定，并覆盖 CHANGELOG 4.x「根级 `$pipeline` 按档分流」条目。

## 4.9.0 (2026-10-25)

### Fixed（严重设计 bug：字段名保留字误伤）

- **`fields` 键空间恢复完全开放（原初设计）**：`naming::conflict` 的 `RESERVED_KEYS`
  保留键检查被移除——field/relation/compute 级契约键（`type`/`required`/`default`/…）
  位于字段定义**对象内部**，与 `fields` 盒子的键（字段名）处于不同对象层，无天然冲突；
  把它们当字段名保留字是一刀切误伤（字段名叫 `type` 的合法 schema 注册即抛
  `ERR_NAME_CONFLICT`）。修复后 `fields: { type: { type: "string", default: "text" } }`
  合法。`detect_conflicts` 仅保留「归一撞名」检测（`orderTotal` vs `order_total` 报错，
  设计 §6.3 本意不变）；字面保留名仅 `_id`（豁免检测）与 `_score`（registry 字面拦截，
  保持现状）。`RESERVED_KEYS` 常量随之移除（core 内部无其他消费方，绑定层未透出）。

### Fixed（归档派生 timestamps 继承）

- **归档派生 def 丢失主 def 的 `timestamps` 键（既有缺陷，同版本修复）**：
  `register_batch` 对每个主 schema 自动派生 `<Name>Deleted` 归档表，派生 def 未携带
  主 def 的 `timestamps` 语义 ⇒ 归档表恒按缺省 `true` 注入 `createdAt`/`updatedAt`。
  主表为 `timestamps:false` 且字段含 `created_at`/`updated_at`（introspect 同步
  snake_case 存量库场景）时，归档构建即归一撞名抛 `ERR_NAME_CONFLICT`
  （`updated_at` vs `updatedAt`）。修复：派生 def 继承主 def 的 `timestamps`
  （缺省仍为 `true`，与 `parse_timestamps` 一致），归档表不再注入重复时间戳列。

## 4.6.2 (2026-10-19)

### Fixed（pg 向量检索查询向量参数）

- **查询向量参数未按 pgvector 文本格式下发**：4.6.1 修复投影后，CI 暴露第二层缺陷——
  `$near` 查询向量的绑定参数是裸数组，asyncpg 拒绝（`expected str, got list`）、
  node-pg 编码成 PG array 字面量 `{1,0.2}`（pgvector 不接受）。修复：与写侧 `bind_col`
  （V-19）同一约定——core 把查询向量序列化为 pgvector 文本输入 `"[1,0.2]"`，
  且 PG 占位符显式 `CAST($n AS vector)`；`score_projection` / `vector_search`
  两份 PG 快照同步更新（SQL 文本 + 参数形态）。

## 4.6.1 (2026-10-19)

### Fixed（pg 原生向量检索投影缺陷）

- **`_score` 派生列被当裸列下发**：SQL 后端向量检索（`$vectorSearch`）的聚合投影循环
  未剔除 `_score`，而 `field_column_ref` 对未声明字段走宽松兜底（按裸列名处理）、
  `physical_of` 保留名白名单不含 `_score` → 命名归一产出 `t."score"` 引用不存在的列，
  PostgreSQL 报 42703 `column t.score does not exist`。修复：投影选列显式剔除 `_score`
  （由向量分支事后补列 `(...) AS "_score"`），并在 `score_projection` 快照补负向回归断言。
  影响面：仅 PG 原生向量路径（pgvector）；MySQL / SQLite 走宿主 `vector_scan` 降级，不受影响。

## 4.4.0 (2026-10-10)

### Fixed（框架缺陷修复-新发现缺陷）

- **N1 JSON 点号路径校验与转义**：JSON 列点号路径补校验与转义，避免路径歧义。
- **N2 关系路径补行条件**：关系子查询路径补齐行级权限条件注入。
- **N6a 联邦无条件注入行条件**：联邦（多源）路径无条件注入行条件。
- **N6d 触发器写补行级探针**：触发器写入路径补行级探针。
- **全量回归**：A2 五路径一致性矩阵，锁定各路径语义一致。

### Added（错误语义档与 machine code 导出）

- **core 错误语义档与 machine code**：core 错误按语义档归一，导出稳定 machine code（含 NoContext 档）。
- **绑定导出 machine code**：`core-py` 暴露 `code` 属性；`core-ffi` 信封补 `code` 字段。

## 4.3.0 (2026-10-09)

### Added（本地磁盘数据源 local + R2 全局单值状态 + 触发器补齐）

- **本地磁盘数据源 `local`**：`DataSource::Local` 与白名单放行，`local/` 子模块（`value.rs` 取值/
  比较/投影、`filter.rs` 谓词求值、`pipeline.rs` 聚合阶段、`update.rs` 与 `eval.rs` 写算子与命令
  分发），聚合补 `$size/$sum/$avg/$min/$max` 表达式算子；`core-node` 透出 `localEval`、`core-py`
  透出 `local_eval`，绑定 parity 自测（local fixtures + 双端回放）。
- **R2 进程级全局单值状态**：core 拆目录快照与策略束 + `with_policy` 基座（结构改造），派生视图与
  覆盖解析 + 快照/覆盖单测（V4 落地），`core-node` / `core-py` 派生视图与回调共享池。
- **schema 触发器补齐**：core `remove` 触发器、core schedule 触发器（cron）、cron 对拍用例
  `cronCases`。

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

## 4.1.1 (2026-10-08)

### Fixed（格式门禁）

- **core 资源模块 rustfmt 格式门禁修复**。

## 4.1.0 (2026-10-05)

### Added（资源数据驱动 core 侧）

- **core 资源纯语义模块**：core 侧资源纯语义（schema / 计算列 / 级联）单测；`core-node` / `core-py`
  绑定透出资源纯函数，双端资源纯函数对拍测试。
- **修复**：core `fmt` 与 `clippy` 门禁（schema 装载代码格式化 + `BatchError` 变体装箱）。

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
