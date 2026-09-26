# rust-store 测试评测报告

> 本报告由 `.trae/skills/test-evaluation` SKILL 流程产出：九维检查项清单 → 全量实跑 → 达成度判定 → 缺陷定级 → 评分。
> 证据根目录（下称 `E`）= `f:\独立开发者\项目\mongo-store\tmp\test-eval\`，全部原始日志落盘于 `E*.log / E*.out`。

## 0. 元信息

| 项 | 值 |
|---|---|
| 评测对象 | `rust-store`（Rust 单核心 + Node/Python 双绑定），版本 **1.0.0**（tag `v1.0.0`；core-node `rust-store-node@1.0.0` / core-py `rust_store_py`），commit **8283e7c** |
| 评测范围 | 全仓（`core/` 全模块 + `core-node/` + `core-py/` + `fixtures/` + `stress/` + `tools/` + `.github/workflows/`），不改动被测代码 |
| 评测维度 | 九维（功能 / 边界 / 组合 / 交互 / 压力 / 兼容 / 安全 / 回归 / 测试工程） |
| 评测环境 | Windows（本机）；Rust stable（cargo test / clippy 实跑）；Node v25.1.0（core-node 测试）；Python 3.14.4（core-py / 压测）；MySQL 8.0.42 / PostgreSQL 16.4 / MongoDB 8.0.12 / SQLite 3.50.4（压测经最小宿主真实驱动） |
| 实测执行 | 约 15 条命令 / 探针（45 项 core 测试 ×5 轮、12 项 node 绑定、8 项 py 绑定、黄金对拍 3 侧复算、51 项 core 探针、clippy 零告警门禁、2 档压测 ×3 轮）；日志见 §7；评测日期 **2026-09-12**（实跑），报告定稿 2026-09-13 |
| 评测标准 | ISO/IEC/IEEE 29119-1..4、ISO/IEC 25010/25023、ISTQB CTFL 4.0、OWASP Top 10 2021 / ASVS / WSTG、MITRE CWE Top 25、F.I.R.S.T、SonarQube Quality Gate、DORA |
| 本轮总分 | **70.4 / 100（C 合格）** |

> **架构前提**：本仓产品 = core 本体（纯逻辑、无 IO）+ 双绑定；宿主层职责（驱动执行、事务）不计入本仓检查项，相关检查项按规划层证据判 d。绑定层等价性以黄金对拍（`fixtures/` + `tools/verify-fixtures.js`）与双绑定 parity 测试判定。nodejs-store / py-store 两份报告对本仓 core 级缺陷（D-01/D-02/D-03/D-06/D-07/D-08）负连带责任，本仓为其**源头**。

## 1. 执行摘要

- **一句话结论**：core 本体工程质量为三仓最高——45 项 core 测试五连跑全绿、clippy `-D warnings` 零告警进 CI、黄金对拍三侧 3/3 一致、hermetic 测试天然并行安全、经最小宿主直压四库 8000 ops 零错误；但两条 Critical **均为本体缺陷**（GQL 关系带参+子选择集解析失败、`$where` 条件静默丢弃），且无任何覆盖率度量，总分落入 B 区间却因 2 条 C 被等级约束压为「合格」。
- **最严重问题（≤3 条）**：
  1. **D-02（C）**：`$where` 恶意载荷使查询条件**静默丢弃**（SQL 路径 `WHERE` 直接消失、`unsupported=[]`），SQLite/Mongo 双后端返回全量数据——结果静默错误，本仓为源头（§4）。
  2. **D-01（C）**：README 记载的「关系带参 + 子选择集」GQL 语法解析失败（`期望 id(undefined) 实际 p({)`），四面（core + 双绑定 + 双宿主）全部复现（§4）。
  3. **D-03（M）**：无 `idPrefix` 且未显式给 `_id` 时生成 `_id=undefined`，Node/Python 双绑定序列化双双崩溃——跨语言绑定边界缺契约（§4）。
- **最突出亮点（≤3 条）**：
  1. 黄金对拍体系：`fixtures/` 冻结基准 + `tools/verify-fixtures.js` 三侧（core / core-node / core-py）复算 **3/3 一致**（`Erust-verify-fixtures.log`），双绑定 parity 20 项全绿。
  2. CI 三层流水线（core：clippy 零告警 + cargo test；node-binding：napi 构建 + parity；py-binding：maturin 构建 + parity，`ci.yml`），clippy 实测 `CLIPPY_EXIT=0`。
  3. hermetic 测试工程：测试无共享库表/端口、cargo 默认多线程并行通过、×5 复跑零 flaky（`Erust-repeat5.log`），压测表后缀 `rs` 隔离。
- **与上一轮对比**：仓库内既有 `doc/2026-09-12-测试报告.md`（记 core「33 个全部通过」，旧口径，来源：`doc/2026-09-12-测试报告.md` §2.1 @ 2026-09-12，本轮未复跑其原始命令——本轮实测 45 项，见 §8）。本轮为**首次九维实跑评测**；性能项与既有压测基线（727.49 QPS）受评测环境干扰不可比（见 §3.5 注）。

## 2. 评分卡

| # | 维度 | 满分 | 达成率 R | 缺陷扣分 P | 维度分 | 主要失分原因 |
|---|------|------|----------|------------|--------|--------------|
| 1 | 功能正确性 | 15 | 0.792 | 5.5 | **6.38** | D-01（C，−5，本体）；幂等性缺回归（m，−0.5）；F5 事务属宿主/F8 无 IO 为设计边界 |
| 2 | 边界值与极值 | 12 | 0.773 | 0.5 | **8.77** | `$limit` 非法类型未校验（m，−0.5）；深度超限语义、时间极值未覆盖 |
| 3 | 组合与等价类 | 10 | 0.800 | 0 | **8.00** | 类型组合/实体状态迁移/异常叠加/剪裁说明仅部分覆盖 |
| 4 | 交互与集成 | 12 | 0.917 | 2.0 | **9.00** | D-03（M，−2，绑定序列化边界）；I6 事务属宿主、I7 替身保真度缺 |
| 5 | 压力与性能 | 12 | 0.750 | 0 | **9.00** | 无 p99、无分段/浸泡、无性能门禁（S10=0）；吞吐基线对照受评测环境干扰不可归因（§3.5 注） |
| 6 | 兼容性 | 10 | 0.727 | 0 | **7.27** | 测试矩阵窄（构建矩阵宽 ≠ 测试矩阵，M9=0）；运行时/后端版本单点验证 |
| 7 | 安全测试 | 12 | 0.750 | 5.5 | **3.50** | D-02（C，−5，本体）；联邦上限错误文案错乱（m，−0.5）；X5 部分覆盖；X9 无审计结论（0） |
| 8 | 回归与质量门禁 | 9 | 0.727 | 0 | **6.55** | 无覆盖率工具（G5=0）；无 CHANGELOG（G7=0.5）；发布前不跑测试（G6=0.5）；分支保护未证实（G9=0.5） |
| 9 | 测试工程与可复现性 | 8 | 0.923 | 0 | **7.38** | 无覆盖率度量（T10=0.5）；README 无统一「如何跑全部测试」（T9=0.5） |
| — | 小计 | 100 | — | 8.0 | **65.85** | |
| — | 亮点加分 | ≤5 | — | — | **+4.50** | 黄金对拍 +2；负向体系 +1.5；独立可并行无 flaky +1 |
| — | **总分** | 100 | — | — | **70.4** | |

等级：**C 合格**（分数落入 B 区间 70-79，但存在 2 条 Critical → 「B 良好」附加约束 `C ≤ 1` 不成立，压为 C）；否决项核查：**无**（V1–V5 均未命中，详见 §3.7）。

## 3. 维度明细

### 3.1 功能正确性（满分 15）

| # | 检查项 | w | d | 加权 | 实测证据（命令/日志） | 说明 |
|---|--------|---|---|------|----------------------|------|
| F1 | 读路径闭环 | 2 | 1.0 | 2.0 | `Erust-core-cargo-test.log`（guards：`query_one_skips_injection_for_custom_pipeline / pushes_limit_one_when_absent / keeps_user_limit` 等 14 项全绿）；探针 S-02 1000 次 `planQuery` 稳定；`prepare_query/strip_query/restore_sort_order` 经 parity 与绑定层验证 | 规划→后处理断言具体行为 |
| F2 | 写路径闭环 | 2 | 1.0 | 2.0 | `parity_write.rs` + `command/mutate/{single,many,upsert}` 模块；探针 X-01/X-02（guest 写/删被拒，错误前缀 `ERR_PERMISSION`） | 写规划含权限与位置信息 |
| F3 | GQL/查询语义 | 2 | 0.5 | 1.0 | 正向：`parity_dialect.rs` 8 项（lookup SQL parity / child limit unsupported 标记 / overlay merge）；反向：**D-01**（`Edefect-d01-recheck.log` V3/V4/V6/V7/V10 全 ERR；探针 B-08 `位置 10`） | 本体解析边界缺陷 |
| F4 | 写入派生语义 | 1 | 1.0 | 1.0 | guards：`timestamps_rejects_invalid_values` / `timestamps_accepts_bool_and_units` | 非法值拒绝 + bool/单位两态 |
| F5 | 事务与多步原子性 | 1 | 0.5 | 0.5 | `pushdown_usecases.rs` b7：跨源 mutation 步骤携带 schema location / namespace（规划层有序可回放） | 事务执行属宿主职责（设计边界）；核心保证步骤完整与定位 |
| F6 | 错误语义 | 1 | 1.0 | 1.0 | `error.rs`：`classify_maps_sentinels_to_variants` / `display_preserves_message_verbatim`；guards：`permission_errors_carry_stable_prefix` | 错误分类与稳定前缀有专测 |
| F7 | 幂等性 | 1 | 0.5 | 0.5 | upsert 幂等经 `parity_write`；归档 upsert-by-`_id` 由宿主层验证 | 重复 register 同名 schema 产生重复条目（**D-06**） |
| F8 | 真实依赖下的正确性 | 1 | 0.5 | 0.5 | core 无 IO（设计约束）；经最小宿主直压四库（`Estress-smoke-tier.log` 联邦形状正确 30 users × orders/invoices 各 2）与宿主仓 E2E 间接验证 | 规划层对真实驱动行为的适配已验 |
| F9 | 跨实现一致性 | 1 | 1.0 | 1.0 | `Erust-verify-fixtures.log`：`Rust core / core-node / core-py` 三侧复算 **3/3 一致** | 冻结黄金基准，本仓为基准所有者 |

- **R** = Σ(w×d)/Σw = 9.5 / 12 = **0.792**；**P** = 5.0（D-01）+ 0.5（D-06）= **5.5**；**维度分** = 15×0.792 − 5.5 = **6.38**
- 本维度缺陷：D-01（C）、D-06（m）

### 3.2 边界值与极值（满分 12）

| # | 检查项 | w | d | 加权 | 实测证据 | 说明 |
|---|--------|---|---|------|----------|------|
| B1 | 数值边界 | 1 | 0.5 | 0.5 | `Erust-probe-bcise.log:13-19`：`$limit=-1/0/1/5000/5001/1e9/3.5` 七档规划行为实测（均未抛错，行为被记录） | `$limit="abc"` 未被校验（**D-07**） |
| B2 | 空与缺省 | 2 | 1.0 | 2.0 | `Erust-probe-bcise.log:3-4`（空串/纯空白 GQL → `期望 id(undefined) 但已到达末尾`）；空结果集 restore 经 parity | 空输入有具体报错断言 |
| B3 | 集合长度边界 | 1 | 0.5 | 0.5 | `parity_federation.rs`：`merge_rejects_oversized_source`（联邦行数上限拒绝） | 批量写入上限无用例（写执行属宿主） |
| B4 | 分页与游标边界 | 2 | 1.0 | 2.0 | `Erust-probe-bcise.log:21-24`：`resolvePage(0,0)/(−1,10)/(1,5001)/(1e9,1e9)` 四边界 → `pageSize` clamp 到 5000，输出逐项断言 | clamp 行为显式验证（三仓唯一） |
| B5 | 字符串边界 | 1 | 1.0 | 1.0 | `Erust-probe-bcise.log:11-12`：B-09 1MiB 超长字段值保真回填；B-10 emoji/中文/零宽/组合字符保真 | 超长 + Unicode 双覆盖 |
| B6 | 时间边界 | 1 | 0.5 | 0.5 | guards：`timestamps_accepts_bool_and_units`（bool/秒/毫秒）+ `rejects_invalid_values` | epoch / 负时间戳 / 时区未覆盖 |
| B7 | 结构深度边界 | 1 | 0.5 | 0.5 | 探针 B-06：深度 10（上限内）构建通过；深度 11 用例因探针仅构造到 L10 报「关系 L11 未定义」（§8 用例构造问题） | 超限降级语义未真正验证 |
| B8 | 状态与资源边界 | 1 | 0.5 | 0.5 | 探针 B-04：未注册 schema → `Schema 未注册: Nope`；B-08 `MAX_PAGINATED_DEPTH` 用例受 D-01 阻断 | 未注册态有断言；超时/断连不适用（无 IO） |
| B9 | 边界断言质量 | 1 | 1.0 | 1.0 | 探针全部断言具体错误串/具体 JSON（如 `{"page":0,"pageSize":5000}`） | 断言具体值，非「不崩溃」 |

- **R** = 8.5 / 11 = **0.773**；**P** = 0.5（D-07）；**维度分** = 12×0.773 − 0.5 = **8.77**
- 本维度缺陷：D-07（m）

### 3.3 组合与等价类（满分 10）

| # | 检查项 | w | d | 加权 | 实测证据 | 说明 |
|---|--------|---|---|------|----------|------|
| C1 | 等价类划分显式性 | 1 | 1.0 | 1.0 | `core/tests/` 按特征分层（guards / parity×7 / pushdown_usecases / route_override）；探针按 BVA/COMB/SEC/INT/STRESS 分组输出 JSON | 划分维度可直接读出 |
| C2 | 多参数组合覆盖 | 2 | 1.0 | 2.0 | `parity_commands / parity_computes / parity_fnfns / parity_federation / parity_write / parity_dialect` 六类矩阵对拍冻结基准 | 命令 × 计算 × 方言组合齐 |
| C3 | 权限/规则决策表 | 2 | 1.0 | 2.0 | 探针 `Erust-probe-bcise.log:27-41`：5 角色 × 读/写 + `requireContext × ctx × 读/写` 8 例决策表（C-r-guest 行为为设计内 fail-open，见 §8） | 含拒绝分支与具体报错 |
| C4 | 状态迁移组合 | 1 | 0.5 | 0.5 | `route_override.rs` 3 态（override / reset-noop / empty-noop）；实体状态迁移（已删再改）无 | 配置态有、实体态无 |
| C5 | 类型组合 | 1 | 0.5 | 0.5 | `dialect_restore_rows_roundtrip_supports_is_array` / bson 类型转换；bool/date/object × 后端不全 | 部分 |
| C6 | 配置/开关组合 | 1 | 1.0 | 1.0 | guards：`setAllowUserPipeline` 禁用后 `planQuery/planFederated` 报错、重开恢复（×2 路径）；`require_context` 开关可逆（×2）；探针 C-up-1/2。（注：`setAllowUserPipeline` 已随归一化 P3「砍 `$pipeline` 逃生舱」移除，本条为历史快照，现状见 rust-store/CHANGELOG.md） | 开关 × 路径全组合 |
| C7 | 异常组合 | 1 | 0.5 | 0.5 | `error.rs` sentinel 分类 + `merge_rejects_oversized_source` | 多失败点叠加无（单命令规划模型，叠加场景属宿主） |
| C8 | 组合剪裁说明 | 1 | 0.5 | 0.5 | `ci.yml` 注释说明三层流水线与 `[lib] test = false` 缘由 | 有分层说明、无组合策略文档 |

- **R** = 8.0 / 10 = **0.800**；**P** = 0；**维度分** = **8.00**
- 本维度缺陷：无

### 3.4 交互与集成（满分 12）

| # | 检查项 | w | d | 加权 | 实测证据 | 说明 |
|---|--------|---|---|------|----------|------|
| I1 | 测试分层结构 | 2 | 1.0 | 2.0 | core 单测 7 + guards 14 + parity 14 + pushdown/route 9 = 45；绑定层 node 12 + py 8——四层清晰 | 绑定 crate `test = false` 由 CI 注释说明 |
| I2 | 契约测试 | 2 | 1.0 | 2.0 | `fixtures/host/{callback_bridge,id_pool,placeholders,truthy}.json` 四契约 + `Erust-verify-fixtures.log` 3/3 + node「未注册的 fn 回调报错」/py callback bridge 消费 | 基准所有者 + 双端消费 |
| I3 | 全链路集成 | 1 | 1.0 | 1.0 | `pushdown_usecases.rs` 6 项：plan → translate → steps 携带 schema location / namespace / 下推判定全链路断言 | |
| I4 | 真实依赖集成 | 2 | 1.0 | 2.0 | `Erust-core-stress-8x200-rerun.log`：最小宿主直压真实 Mongo/MySQL/PG/SQLite 四库 8000 ops **0 errors**，联邦结果形状正确 | 真实驱动下规划层行为验证 |
| I5 | 异步与回调交互 | 1 | 1.0 | 1.0 | `parity_fnfns.rs`；node `未注册的 fn 回调报错`（`Erust-corenode-test.log`）；py `test_callback_bridge_fn_and_asyncfn`（py-store 侧消费） | fn/asyncFn 双形态 |
| I6 | 事务与连接交互 | 1 | 0.5 | 0.5 | 无事务 API（宿主职责）；步骤顺序/位置信息在规划层已验（pushdown b7） | 设计边界，非缺陷 |
| I7 | 测试替身保真度 | 1 | 0.5 | 0.5 | 测试为纯逻辑对拍、无替身 | **D-03** 证明「绑定序列化边界」无契约覆盖（`Edefect-d03-rust-binding.log`） |
| I8 | 跨进程/并发交互 | 1 | 1.0 | 1.0 | 探针 I-01（多 Registry 开关隔离 `aBlocked=true bOk=true bPipelineBlocked=true`）/ I-02（混合读/写/联邦并发无交叉污染）/ S-02（1000 次连续 planQuery 稳定） | |
| I9 | E2E 数据隔离与可重复 | 1 | 1.0 | 1.0 | 测试用冻结 JSON fixtures、无共享库表；压测表后缀 `rs` 隔离；×5 复跑零漂移 | hermetic，天然隔离 |

- **R** = 11.0 / 12 = **0.917**；**P** = 2.0（D-03）；**维度分** = 12×0.917 − 2.0 = **9.00**
- 本维度缺陷：D-03（M）

### 3.5 压力与性能（满分 12）

| # | 检查项 | w | d | 加权 | 实测证据 | 说明 |
|---|--------|---|---|------|----------|------|
| S1 | 压测资产可运行 | 2 | 1.0 | 2.0 | `Erust-core-stress-8x200-rerun.log` / `Estress-smoke-tier.log`（`stress/stress_core.py` 输出 `[RESULT]` JSON） | 直压 core-py，含联邦链路分项 |
| S2 | 负载档位 | 1 | 1.0 | 1.0 | 冒烟 1×10（268.35）+ 负载 8×200 两轮（327.75 / 279.57） | 两档 × 多轮 |
| S3 | 指标完整性 | 1 | 1.0 | 1.0 | `qps / errors / errors_by_type / all_ops{p50,p95,max,avg} / federation_ops{...}` | 缺 p99 |
| S4 | 错误率 | 2 | 1.0 | 2.0 | 全部轮次 `errors=0`（8000 ops ×3） | 无错误可归因 |
| S5 | 吞吐基线对照 | 1 | 0.5 | 0.5 | 历史基线 **727.49 QPS**（`doc/2026-09-12-压测报告.md:29` @ 2026-09-12，本轮未复跑）→ 本轮 **279.57–327.75**；同轮对标 py-store 307.84 / nodejs-store 560.62 | 评测时段机器被外部任务占满（CPU 100%、磁盘近满，用户确认）→ 与基线差异**不可归因于产品**，仅记录实测值 |
| S6 | 尾延迟分析 | 1 | 0.5 | 0.5 | `all_ops p50=21.86 / p95=64.90 / max=377.60 ms`；联邦 `p50=56.76 / p95=88.18` | 无 p99；既有报告「core 计算路径延迟分布紧凑，无长尾毛刺」（`doc/2026-09-12-压测报告.md:52`，本轮未复跑该结论的环境） |
| S7 | 瓶颈定位 | 1 | 0.5 | 0.5 | 既有归因「联邦 p50 中主要为四库 IO 与逐源往返；双宿主差异来自宿主侧运行时」（`doc/2026-09-12-压测报告.md:59-61`） | 无 CPU/内存观测证据 |
| S8 | 稳定性与泄漏 | 1 | 0.5 | 0.5 | 两轮独立 8×200（327.75 / 279.57）+ 探针 S-02 1000 次稳定 | 无前/中/后分段或浸泡 |
| S9 | 压测可信度 | 1 | 1.0 | 1.0 | 原始 `[RESULT]` 行落盘、命令可复现、表/库隔离（suffix `rs`） | |
| S10 | 性能门禁 | 1 | 0.0 | 0.0 | `ci.yml` 无压测 job | 无阈值断言 |

**吞吐对照表**

| 档位 | 规模 | QPS | errors | p50 (ms) | p95 (ms) | max (ms) |
|---|---|---|---|---|---|---|
| 冒烟 | 1×10 | 268.35 | 0 | 1.22 | 13.25 | 14.56 |
| 负载 | 8×200 | 327.75 / 279.57（两轮） | 0 | 18.50 / 21.86 | 56.12 / 64.90 | 266.95 / 377.60 |
| 历史基线 | 8×200 | **727.49**（来源：`doc/2026-09-12-压测报告.md:29` @ 2026-09-12，**本轮未复跑**） | — | — | — | — |

> 注：评测时段机器被外部任务占满（CPU 100%、磁盘近满，用户确认），三仓吞吐与历史基线的差异不作产品缺陷计分（见 §6 / §8）。

- **R** = 9.0 / 12 = **0.750**；**P** = 0；**维度分** = **9.00**
- 本维度缺陷：无

### 3.6 兼容性（满分 10）

| # | 检查项 | w | d | 加权 | 实测证据 | 说明 |
|---|--------|---|---|------|----------|------|
| M1 | 多后端等价性 | 2 | 1.0 | 2.0 | `dialect_cross_backend_sql_parity` + 直压四库 0 errors（`Erust-core-stress-8x200-rerun.log`） | 四后端翻译与执行等价 |
| M2 | 方言差异专项 | 2 | 1.0 | 2.0 | `parity_dialect.rs` 8 项：`dialect_aggregate_lookup_sql_parity` / `dialect_translate_child_limit_marked_unsupported` / `dialect_overlay_merge_compute_and_permission` / `dialect_restore_rows_roundtrip_supports_is_array` / `dialect_introspection_to_schema_json` 等 | 方言特性矩阵显式成套 |
| M3 | 多语言绑定等价性 | 1 | 1.0 | 1.0 | `Erust-verify-fixtures.log` 3/3 一致 + node 12 / py 8 parity 全绿 | 本仓为基准所有者 |
| M4 | 运行时版本兼容 | 1 | 0.5 | 0.5 | CI：node 20 / python 3.12；本地实测：node v25.1.0 / python 3.14.4——各两版本 | 无版本矩阵 |
| M5 | 平台兼容 | 1 | 0.5 | 0.5 | release-npm 5 平台矩阵 + release-pypi manylinux/musllinux/windows/macos——但**测试**仅 ubuntu CI + Windows 本机 | 构建矩阵宽 ≠ 测试矩阵 |
| M6 | 后端版本兼容 | 1 | 0.5 | 0.5 | `Eenv-versions.log`：MySQL 8.0.42 / PG 16.4 / Mongo 8.0.12 / SQLite 3.50.4 | 仅单版本，无范围声明 |
| M7 | 编码与排序规则兼容 | 1 | 1.0 | 1.0 | 探针 B-10：Unicode（emoji/中文/零宽/组合字符）保真回填；B-09 1MiB 保真 | 本体层面显式覆盖 |
| M8 | 向后兼容 | 1 | 0.5 | 0.5 | 冻结 parity 快照可回归旧行为 | 无「旧用法零变更」专项 |
| M9 | 兼容矩阵自动化 | 1 | 0.0 | 0.0 | `ci.yml` 单 OS（ubuntu）、无 DB/版本矩阵 | 纯单点验证 |

- **R** = 8.0 / 11 = **0.727**；**P** = 0；**维度分** = **7.27**
- 本维度缺陷：无

### 3.7 安全测试（满分 12）

| # | 检查项 | w | d | 加权 | 实测证据 | 说明 |
|---|--------|---|---|------|----------|------|
| X1 | 注入防御 | 2 | 0.5 | 1.0 | 正向：探针 X-04（NoSQL 注入载荷 `$where/$function` 在 condition 中 **REJECT ×4**）、X-05（GQL 注入：注释/多语句/闭合符逃逸 → `ok(0)×3`）；反向：**D-02**——`Edefect-d02-unsupported.log`：`$where` 使 SQL `WHERE` 静默消失、`unsupported=[]` | 注入载荷在 condition 通道被拒，但在 pipeline 通道静默丢弃 |
| X2 | 权限矩阵 | 2 | 1.0 | 2.0 | 探针 X-01/X-02（`ERR_PERMISSION:无写入权限/无删除权限`）+ 5 角色读/写决策表 + `test_perm_*`（宿主侧消费验证） | 含拒绝分支与稳定错误前缀 |
| X3 | 越权防御 | 2 | 1.0 | 2.0 | 探针 X-06（creator 读自动附加 createdBy 过滤）、X-08（`internal=true` 不可伪造 `allow(roles=[internal])`）、`route_override.rs`（override 仅作用于命令面）；X-07 角色字符串未归一化为设计内 fail-open 契约（§8） | 水平/垂直越权有负向验证 |
| X4 | fail-secure 默认 | 1 | 1.0 | 1.0 | guards：`require_context_blocks_read_paths_without_ctx / _write_paths / system_context_passes / switch_is_reversible` ×4 | fail-secure 可开可关、有专测 |
| X5 | 恶意载荷 | 1 | 0.5 | 0.5 | **D-02**：`$where` 在查询 pipeline 通道未拦截；超深 GQL / 超大批量未覆盖 | 载荷防御存在通道盲区 |
| X6 | 敏感信息暴露 | 1 | 1.0 | 1.0 | `permission_errors_carry_stable_prefix`；错误消息业务语义，无 SQL 原文/内部路径 | |
| X7 | 资源耗尽防御 | 1 | 0.5 | 0.5 | `resolvePage` 四边界 clamp 5000 + `merge_rejects_oversized_source`；但超限错误文案错乱（**D-08**：`第 0 个取数单元的结果必须是数组`） | 上限存在、错误语义不符 |
| X8 | 凭据与配置安全 | 1 | 1.0 | 1.0 | 仓库无凭据；workflow 一律走 `secrets` / OIDC Trusted Publishing | 风险等级：低 |
| X9 | 依赖漏洞 | 1 | 0.0 | 0.0 | `Erust-cargo-audit-install.log`：cargo-audit 安装未完成（仅见编译日志，无审计结论） | 受限清单 §6；core 依赖面小但不能替代审计 |

- **R** = 9.0 / 12 = **0.750**；**P** = 5.0（D-02）+ 0.5（D-08）= **5.5**；**维度分** = 12×0.750 − 5.5 = **3.50**
- 本维度缺陷：D-02（C）、D-08（m）
- **否决项核查**：V1 要求「越权读到他人数据」。`Edefect-d02-v1-mongo.log` 实测 Mongo 原生路径带 owner 条件的攻击载荷**未**读到他人数据（`越权可见他人=false`，报 `JS functions cannot be represented as a serde_json.Value`）；D-02 构成「条件静默丢弃 / 结果静默错误」，按 §3 判 **C**，不构成 V1。

### 3.8 回归与质量门禁（满分 9）

| # | 检查项 | w | d | 加权 | 实测证据 | 说明 |
|---|--------|---|---|------|----------|------|
| G1 | 测试套件全绿 | 2 | 1.0 | 2.0 | `Erust-core-cargo-test.log` 45 passed / 0 failed；`Erust-corenode-test.log` 12 pass；`Erust-corepy-test.log` 8 passed；`Erust-repeat5.log` 5×exit 0 | 三层全绿 ×5 |
| G2 | 回归基线 | 1 | 1.0 | 1.0 | `fixtures/` 冻结快照 + `tools/verify-fixtures.js` 复算 3/3（`Erust-verify-fixtures.log`） | 基准所有者，含生成与消费链路 |
| G3 | 门禁完整性 | 2 | 1.0 | 2.0 | `ci.yml`：`cargo clippy --workspace --all-targets -- -D warnings`（lint）+ `cargo test -p rust-store-core`（test）双门禁；实测 `CLIPPY_EXIT=0` | lint + test 齐备（覆盖率门槛缺，见 G5） |
| G4 | CI 与本地一致性 | 1 | 1.0 | 1.0 | CI 三层与本地完全同命令；core 测试无 DB 依赖、无 skip 盲区；绑定 parity 由 CI 真实构建验证 | 三仓唯一「无恒 skip 盲区」 |
| G5 | 覆盖率门槛 | 1 | 0.0 | 0.0 | 无 tarpaulin / llvm-cov 等任何覆盖率工具 | 无度量即无门槛 |
| G6 | 发布流程校验 | 1 | 0.5 | 0.5 | `release-npm.yml` / `release-pypi.yml` 均直接构建发布，**发布前不跑测试** | 半覆盖 |
| G7 | 变更可追溯 | 1 | 0.5 | 0.5 | git log 6 条全 Conventional Commits；tag `v1.0.0` ↔ core-node 1.0.0；**无 `CHANGELOG.md`** | 半覆盖 |
| G8 | 缺陷回归固化 | 1 | 0.5 | 0.5 | `guards.rs` 14 项固化行为契约（require_context / timestamps / user pipeline 开关）；但本轮 D-01/D-02 无回归用例 | 半覆盖 |
| G9 | 失败阻断能力 | 1 | 0.5 | 0.5 | `ci.yml` 在 `push: main` + `pull_request` 触发 | 分支保护配置未证实 |

- **R** = 8.0 / 11 = **0.727**；**P** = 0；**维度分** = **6.55**
- 本维度缺陷：无

### 3.9 测试工程与可复现性（满分 8）

| # | 检查项 | w | d | 加权 | 实测证据 | 说明 |
|---|--------|---|---|------|----------|------|
| T1 | 独立性 | 2 | 1.0 | 2.0 | `cargo test -p rust-store-core`、`node --test`、`pytest core-py/test/parity_test.py` 三层各自独立可跑（日志均 exit 0） | 全部文件独立验证 |
| T2 | 可重复性 | 2 | 1.0 | 2.0 | `Erust-repeat5.log`：run 1..5 exit=0，`total_passed=45 non_green_lines=0` | flaky 率 = 0 |
| T3 | 并行安全 | 1 | 1.0 | 1.0 | `cargo test` 默认多线程并行执行通过（45 项）；绑定测试 hermetic | 无共享状态冲突 |
| T4 | 数据隔离 | 1 | 1.0 | 1.0 | 测试消费冻结 JSON fixtures、不触共享库表；压测独立后缀 `rs` | 隔离机制成立 |
| T5 | 可移植性 | 1 | 1.0 | 1.0 | ubuntu CI + Windows 本机双环境全绿；无仓库外硬路径（stress 经相对路径定位 `core-py/dist`） | |
| T6 | 可维护性 | 1 | 1.0 | 1.0 | `core/src` 模块树清晰（command/computes/dialect/federation/pipeline 分层）；guards 按行为命名 | |
| T7 | 断言质量 | 2 | 1.0 | 2.0 | parity 对拍冻结基准为**精确匹配**断言；guards 断言具体错误前缀/行为 | |
| T8 | 执行时长 | 1 | 1.0 | 1.0 | core cargo test 秒级；core-node 236 ms；core-py 0.08 s | 远低于 5 min |
| T9 | 测试文档 | 1 | 0.5 | 0.5 | README 有「对拍测试」（`parity*.rs` + `parity.test.js` 覆盖面说明）与「守卫测试见 core/tests/guards.rs」（`README.md:36-50`）；无统一「如何跑全部测试」章节 | 部分具备 |
| T10 | 覆盖广度 | 1 | 0.5 | 0.5 | 45 core 用例 + 20 绑定用例；**无覆盖率度量佐证广度** | 受限于无工具 |

- **R** = 12.0 / 13 = **0.923**；**P** = 0；**维度分** = **7.38**
- 本维度缺陷：无

## 4. 缺陷清单

| ID | 严重度 | 维度 | 标题 | 最小复现步骤 | 原始输出证据 | CWE/规则 | 影响面 | 修复建议 |
|----|--------|------|------|--------------|--------------|----------|--------|----------|
| D-01 | **C** | 1 | GQL「关系带参 + 子选择集」解析失败（**本体缺陷**） | `node tmp/test-eval/probes/_verify15.cjs`，观察 V3/V4/V6/V7/V10 | `Edefect-d01-recheck.log`：`V3 ERR 期望 id(undefined) 实际 p({) 位置 10`、`V4 ERR 位置 18`；探针 B-08 同因；Python 侧 `Edefect-d01-recheck-py.log` 同判（位置 10/15） | CWE-20 | 四面（core + 双绑定 + 双宿主）；README 记载语法不可用 | 修正 GQL 语法分析：关系名后「参数列表 + 选择集」需可同时出现；补正向回归用例进 `core/tests/` |
| D-02 | **C** | 7 | `$where` 恶意载荷使查询条件静默丢弃、返回全量数据（**本体缺陷**） | `node tmp/test-eval/probes/probe-js.cjs`（X-03，sqlite 与 mongo 双路径） | `Ejs-probe-bcise.log:11,21` / `Epy-probe-bcise.log:11,21`：`$where 载荷导致全量返回，rows=85`；SQL 侧条件静默消失：`Edefect-d02-unsupported.log`（`SELECT t."_id", t."name" FROM "v_parent" t`，`unsupported=[]`） | CWE-89 / CWE-943 / OWASP A03:2021 | 四面；结果静默错误（调用方无法区分「无数据」与「条件被丢弃」） | 不支持的条件键应**显式报错**而非静默丢弃；对 `$where`/`$function` 等键建立拒绝名单；补负向回归用例进 `core/tests/` |
| D-03 | **M** | 4 | 无 `idPrefix` 且未显式给 `_id` 时生成 `_id=undefined`，跨绑定序列化崩溃 | `node tmp/test-eval/probes/_verify11.cjs`；Python 侧 `_verify_py2.py` | `Eprobe-iso11.log`：`insert._id = undefined` → `query ERROR JS functions cannot be represented as a serde_json.Value`；`Eprobe-py-iso2.log`：`TypeError: 不支持的 Python 类型`；`Edefect-d03-rust-binding.log` | CWE-20（跨语言绑定序列化边界） | 四面（核心默认值协商 + 绑定层） | `_id` 缺失且无 `idPrefix` 时核心显式报错或由绑定层统一兜底生成；补双向序列化契约用例 |
| D-06 | m | 1 | 重复 register 同名 schema 静默覆盖且 `list()` 出现重复条目 | `node tmp/test-eval/probes/probe-rust.cjs`（I-03） | `Erust-probe-bcise.log:72`：`list=4 second=ok`（重复注册后列表为 `["Dup","DupDeleted","Dup","DupDeleted"]`） | CWE-20 | 本体 + 全部下游；Host 枚举 schema 时重复 | 同名覆盖时同步去重 `order`，或显式报错 |
| D-07 | m | 2 | `$limit` 非数值未校验，原样进入聚合管道 | `probe-rust.cjs`（B-18） | `Erust-probe-bcise.log:68`：`{"$limit":"abc"}` 被原样放入 `pipeline` | CWE-20 | 本体 + 全部下游 | 对 `$limit/$skip` 做类型与范围校验，非法值显式报错 |
| D-08 | m | 7 | 联邦结果超 `MAX_FEDERATION_ROWS` 时错误文案错乱 | `probe-rust.cjs`（S-01） | `Erust-probe-bcise.log:75`：`第 0 个取数单元的结果必须是数组`（未提及行数上限） | CWE-703 | 本体 + 全部下游；排障成本上升 | 超限时抛出含上限与实测行数的专用错误 |

**严重度统计**：B **0** / C **2** / M **1** / m **3** / I **0**

## 5. 测试覆盖矩阵（九维 × 现状）

| 维度 | 既有资产 | 本轮实跑 | 补充实跑 | 缺口结论 |
|------|----------|----------|----------|----------|
| 1 功能 | 45 项（guards 14 + parity×7 + pushdown 6 + route 3 + 单测 7） | 有 | 有（51 探针 + 缺陷复现） | D-01 解析边界；事务执行属宿主 |
| 2 边界 | guards（timestamps）+ resolvePage（隐含） | 有 | 有（BVA 22 例） | 深度超限语义、时间极值缺 |
| 3 组合 | parity 六类矩阵 + guards | 有 | 有（决策表 15 例） | 类型组合/实体状态迁移缺 |
| 4 交互 | 双绑定 parity + host 契约 fixtures | 有 | 有（并发/隔离探针 + 四库直压） | 事务执行属宿主；替身保真度缺 |
| 5 压力 | `stress/stress_core.py` | 有（2 档 ×3 轮） | 有 | 无门禁/p99/浸泡；基线对照受环境干扰 |
| 6 兼容 | dialect parity 四后端 + Unicode 探针 | 有 | 有（版本实采） | 测试矩阵窄，M9=0 |
| 7 安全 | guards（require_context/permission 前缀）+ 探针注入组 | 有 | 有（注入/越权 1 轮） | `$where` pipeline 通道盲区；cargo-audit 未完成 |
| 8 回归门禁 | `ci.yml` 三层 + clippy 门禁 + fixtures | 有 | 有 | 无覆盖率工具；发布前不跑测试；无 CHANGELOG |
| 9 测试工程 | 45+12+8 用例、hermetic、可并行 | 有 | 有（×5 复跑、并行） | 无覆盖率度量；README 无统一运行说明 |

## 6. 受限清单（未执行的检查项）

| 检查项 | 未执行原因 | 建议补测方式 |
|--------|------------|--------------|
| 安全：cargo-audit 依赖审计（X9） | 工具安装未完成（`Erust-cargo-audit-install.log` 仅见编译日志，无审计结论） | `cargo install cargo-audit` 成功后复跑 `cargo audit` |
| 覆盖率：语句/分支度量（G5/T10） | 仓库无 tarpaulin/llvm-cov 设施 | CI 接入 `cargo-llvm-cov` 并设阈值 |
| 压力：尖峰 / 浸泡档 | 需用户同意长时占用机器 | 低负载 × ≥30 min 浸泡 + 尖峰跳变 |
| 压力：吞吐基线复测（S5） | **评测时段机器被外部任务占满（CPU 100%、磁盘近满，用户确认）**，与历史基线 727.49 的差异不可归因于产品 | 空闲时段复跑 `python stress/stress_core.py --workers 8 --rounds 200` 后再对照 |
| 压力：p99 指标 | `stress_core.py` 未输出 p99 | 脚本补 p99 分位 |
| 压力：性能门禁（S10） | CI 无压测 job | `ci.yml` 增 `stress` job 并对 QPS/尾延迟设阈值断言 |
| 兼容：OS/运行时/后端版本测试矩阵（M4/M5/M6/M9） | 本机仅 1 组版本；CI 仅 ubuntu 单平台 | `ci.yml` 增 OS × Node/Python matrix；DB 兼容由宿主仓矩阵覆盖 |
| 交互：事务执行语义（I6） | core 无 IO、无事务 API（设计边界） | 事务语义验证归属宿主仓评测（nodejs-store / py-store 报告 I6） |

## 7. 执行证据附录

> `E` = `f:\独立开发者\项目\mongo-store\tmp\test-eval\`

```
[维度 1/4/8] $ cargo test -p rust-store-core
  结果：45 passed / 0 failed（guards 14 / parity_dialect 8 / pushdown_usecases 6 / route_override 3 /
        datasource 4 / error 3 / parity 1×5 / federation 2；exit 0）
  日志：Erust-core-cargo-test.log

[维度 9] $ cargo test -p rust-store-core ×5（重复性）
  结果：run 1..5 exit=0 :: total_passed=45 non_green_lines=0
  日志：Erust-repeat5.log

[维度 8] $ cargo clippy --workspace --all-targets -- -D warnings
  结果：CLIPPY_EXIT=0（零告警）
  日志：Erust-clippy.log

[维度 4/8] $ node --test "test/**/*.test.js"  （core-node 绑定）
  结果：tests 12 / pass 12 / fail 0 / duration 236.5 ms（exit 0）
  日志：Erust-corenode-test.log

[维度 4/8] $ python -m pytest core-py/test/parity_test.py -v  （core-py 绑定）
  结果：8 passed in 0.08s
  日志：Erust-corepy-test.log

[维度 4] $ node tools/verify-fixtures.js  （黄金对拍三侧复算）
  结果：Rust core 一致 / core-node 绑定 一致 / core-py 绑定 一致 → 3/3
  日志：Erust-verify-fixtures.log

[维度 2/3/4/7] $ node tmp/test-eval/probes/probe-rust.cjs  （core 探针，51 例）
  结果：total=51 pass=40 fail=11
  日志：Erust-probe-bcise.log

[维度 5] $ python stress/stress_core.py --workers 8 --rounds 200  （直压 core-py）
  结果：{"qps":327.75,"errors":0,"all_ops":{"p50_ms":18.50,"p95_ms":56.12,"max_ms":266.95}}
        （复跑 279.57——时段受外部负载干扰，见 §3.5 注）
  日志：Erust-core-stress-8x200-rerun.log / Erust-core-stress-8x200.log / Estress-8x200-idle-rerun.log

[维度 5] 冒烟档 1×10
  结果：{"qps":268.35,"errors":0}
  日志：Estress-smoke-tier.log

[维度 6] 环境与后端版本实采
  结果：python=3.14.4 / mysql=8.0.42 / postgres=16.4 / mongo=8.0.12 / sqlite=3.50.4 / node=v25.1.0
  日志：Eenv-versions.log

[维度 8] 仓库元信息与 CI/发布流水线
  结果：workflows = ci.yml / release-npm.yml / release-pypi.yml；git log 6 条；tags v1.0.0 / v0.1.0
  日志：Erepo-git-and-workflows.log

[维度 1/2/7] 缺陷最小复现复核
  结果：D-01（V3/V4/V6/V7/V10 全 ERR）、D-02（SQL 条件静默消失 + Mongo 原生路径未越权）、D-03（双绑定崩溃）
  日志：Edefect-d01-recheck.log / Edefect-d01-recheck-py.log / Edefect-d02-unsupported.log /
        Edefect-d02-v1-mongo.log / Edefect-d03-rust-binding.log / Eprobe-iso11.log / Eprobe-py-iso2.log

[维度 7] cargo-audit 安装尝试
  结果：安装未完成（仅编译日志，无审计结论）→ 受限清单 §6
  日志：Erust-cargo-audit-install.log
```

## 8. 范围外发现（不计分）

| 现象 | 位置 | 初判严重度 | 建议 |
|------|------|------------|------|
| 角色字符串未归一化 + schema **未配置白名单**时默认放行（`"admin "`/`"ADMIN"`/`""`/`"Admin"`/`"super_admin "` 均 allow；guest 读被放行）——经代码复核为 fail-open 设计契约 | `core/src/permission.rs:71-89` | I（设计内） | 文档化默认放行契约；建议角色字符串归一化并显式拒绝空串 |
| 未声明字段在投影中被静默丢弃（探针 B-05 期望报错） | `pipeline/projection` 规划层 | I（待确认契约） | 明确「未声明字段忽略」是否为公开契约，若是则补文档 |
| `register` 缺 `collection` 时回落为 `name`（探针 I-04 期望拒绝） | `core/src/schema.rs:119-124` | I（设计内） | 文档化默认值语义 |
| 深度 11 用例报「关系 L11 未在 schema L10 中定义」（探针仅构造到 L10） | 探针 B-07 用例构造 | I（用例构造问题） | 修正探针 schema 层级后复测超限降级语义 |
| `require_context=true` 且无 ctx 时抛 `ERR_NO_CONTEXT`（探针 C-rc-1/2 误判为「意外报错」） | guards 行为 | I（非缺陷，实为 fail-secure 生效） | 修正探针断言后复测 |
| `$limit` 负数/0/小数被规划层静默接受（B-11..B-17 均未抛错） | plan 层 | I（设计内） | 文档化规划语义（宿主可再校验） |
| `doc/2026-09-12-测试报告.md` 记 core「33 个全部通过」，本轮实测 **45** 项（guards/parity 扩充后口径） | `doc/2026-09-12-测试报告.md` §2.1 | I | 更新文档口径 |
| 评测时段机器被外部任务占满（CPU 100%、磁盘近满，用户确认）：三仓吞吐全部显著低于历史基线（rust −61.6%），差异属环境干扰而非产品退化 | 全部压测日志（`E*-stress-*.log`） | I（环境） | 空闲时段复测后再与基线对比 |

## 9. 改进建议（按优先级）

| 优先级 | 建议 | 对应缺陷 | 预期收益 | 落地方式 |
|--------|------|----------|----------|----------|
| P0 | `$where`/`$function` 等不支持条件键改为**显式报错**，绝不静默丢弃 | D-02 | 消除「结果静默错误」，四面同步受益 | core `pipeline/plan` 校验 + 拒绝名单 + 回归用例 |
| P0 | 修正 GQL 语法：关系名后「参数 + 选择集」可同时出现 | D-01 | 恢复 README 记载能力 | core parser + 正向/反向用例 |
| P1 | `_id` 缺失且无 `idPrefix` 时显式报错或绑定层统一兜底 | D-03 | 消除跨绑定崩溃 | core + 双绑定序列化契约用例 |
| P1 | CI 接入覆盖率工具并设门槛 | G5/T10 | 度量从 0 到 1，支撑广度结论 | `ci.yml` core job 增 `cargo-llvm-cov` |
| P1 | 发布 workflow 发布前跑测试 | G6 | 阻断坏版本发布 | `release-npm.yml` / `release-pypi.yml` 增 test step |
| P2 | `$limit/$skip` 类型与范围校验；联邦上限专用错误 | D-07/D-08 | 边界输入可控、排障成本下降 | core 校验 + 用例 |
| P2 | 增加性能门禁 job（QPS/尾延迟阈值断言）；空闲时段复测吞吐建基线 | S10/§8 | 性能回归可自动拦截 | `ci.yml` 增 `stress` job |
| P2 | 新建 `CHANGELOG.md`；文档化角色 fail-open 契约与 `$limit` 规划语义 | G7/§8 | 变更可追溯、契约清晰 | docs |

## 10. 复现指南

```powershell
# 环境前置：Windows；Rust stable（rustup）；Node v25.1.0；Python 3.14.4
# 数据库（压测/宿主 E2E 需要）：MySQL 8.0.42 / PostgreSQL 16.4 / MongoDB 8.0.12（凭据 e2e/e2e123，库 mongo_store_e2e）
cd f:\独立开发者\项目\mongo-store\rust-store

# 1. core 测试 + clippy 门禁
cargo test -p rust-store-core
cargo clippy --workspace --all-targets -- -D warnings

# 2. node 绑定 parity
cd core-node
npm ci
npx napi build --platform
npm test
cd ..

# 3. py 绑定 parity
cd core-py
pip install maturin pytest
maturin build --out dist
pip install --force-reinstall dist/*.whl
python -m pytest test/parity_test.py -v
cd ..

# 4. 黄金对拍三侧复算（需三侧产物已构建）
node tools\verify-fixtures.js

# 5. 压测（core-py 直压，需四库可达；空闲时段运行以获得可比基线）
$env:LOCAL_CORE='1'
python stress\stress_core.py --workers 8 --rounds 200    # 负载档
python stress\stress_core.py --workers 1 --rounds 10     # 冒烟档

# 6. 补充探针（core 直驱，51 例）
node f:\独立开发者\项目\mongo-store\tmp\test-eval\probes\probe-rust.cjs
```

## 附录 A：标准对照

| 标准 | 本报告的使用位置 |
|------|------------------|
| ISO/IEC/IEEE 29119-1..4 | 九维框架、BVA/等价类/决策表设计（§3.2/3.3） |
| ISO/IEC 25010 / 25023 | 维度 1/5/6/7 的质量特性与度量 |
| ISTQB CTFL 4.0 | 维度 2/3 的边界值、等价类、状态迁移 |
| OWASP Top 10 2021 / ASVS / WSTG | 维度 7 攻击面（A03 注入、通道盲区） |
| MITRE CWE Top 25 | §4 各缺陷 CWE 归类 |
| F.I.R.S.T / Clean Tests | 维度 9 判据（hermetic/并行/快速反馈） |
| SonarQube Quality Gate / DORA | 维度 8 门禁与性能回归判据 |

## 附录 B：项目规则优先声明

- 绑定 crate 已设 `[lib] test = false`（cdylib 测试二进制无宿主即 `STATUS_DLL_NOT_FOUND`，`ci.yml:8-9` 注释）——`cargo test --workspace` 的误判不算缺陷，测试职责按「core 单测 + 绑定层 parity」分层判定，以项目分层设计为准。
- 绑定 parity 测试使用 debug 构建、**不测性能**（`ci.yml:51` 注释「parity 不测性能」）——性能结论以 `stress/stress_core.py` 专项为准。
- 本仓产品 = core 本体（纯逻辑无 IO）：宿主层职责（驱动执行、事务、连接管理）不计入本仓检查项，相关检查项（F5/F8/I6）按规划层证据判 d 并注明设计边界。
- 压测模型「最小宿主」（`stress/stress_core.py` docstring）：仅命令路由 + 驱动 IO + ID 随机源，与 py-store / nodejs-store 压测操作模型对称——跨仓吞吐对比仅作参考，本报告不据此下产品性能结论（评测时段环境干扰，见 §3.5 注）。
