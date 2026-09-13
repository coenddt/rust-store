# 代码评审评测报告：多后端归一化 P5 + RBAC R0 未提交变更

> 评测轮次：第 1 轮
> 评测时间：2026-09-13
> 评测对象：变更评审（三仓库**未提交工作区 diff**，含未跟踪新文件）
> - `rust-store`：core + core-node/core-py 薄绑定 + fixtures + tests（52 改 + 5 新增，约 +5140/−1588）
> - `py-store`（27 改 + 2 新增，约 +522/−809）
> - `nodejs-store`（19 改 + 1 新增，约 +192/−143）
> 技术栈：Rust（core 纯逻辑；napi-rs / PyO3 双绑定；MySQL/PostgreSQL/SQLite 方言）、Node.js（CommonJS，ESLint）、Python（PyMongo，无 lint 配置）
> 评测口径：生产系统默认权重（无降权）；仅对「本次变更引入 / 触碰」的问题计分，历史问题列「范围外发现」

## 评测范围与取证命令

只读评审，**未修改任何源码**。取证命令（PowerShell，逐仓库执行）：

```powershell
git -C rust-store    status --short
git -C rust-store    diff
git -C rust-store    diff --stat
git -C py-store      status --short ; git -C py-store   diff ; git -C py-store   diff --stat
git -C nodejs-store status --short ; git -C nodejs-store diff ; git -C nodejs-store diff --stat
# 未跟踪新文件逐个 Read（本报告已逐一读取判断）
#   rust-store/core/src/pipeline/group.rs（469 行）、relation_filter.rs（605 行）
#   rust-store/core/src/dialect/select/group_agg.rs（369 行）、relation_agg.rs（248 行）
#   rust-store/doc/fix-plan/2026-09/RBAC-R0拍板与落地范围.md
#   nodejs-store/src/executors/mongo.js（102 行）
#   各仓库 .trae/skills/*/SKILL.md（rust 247 行 / py 200 行 / node 193 行）
# lint 证据（项目规则优先）：
cargo clippy -p rust-store-core --all-targets --message-format short   # → Finished，零警告
npm run lint                                                          #（nodejs-store）→ eslint 通过，无输出
# py-store 无 lint 配置
```

未跟踪文件（`??`）已全部纳入本次评测范围：`group.rs`、`relation_filter.rs`、`group_agg.rs`、`relation_agg.rs`、`RBAC-R0拍板与落地范围.md`、`nodejs-store/src/executors/mongo.js`、三仓库 `.trae/skills/**`。

本轮变更主题（理解上下文，不免检）：① 归一化 P5（根级 `$group`/`$having`、计算列 `agg` 与每父 top-N 窗口、§9.6 关系聚合谓词 semi/anti-join）；② RBAC R0 落地（不可读表/关系/派生值 → `Err(ERR_PERMISSION)`）；③ SQL 后端 object/array 字段读写不再静默丢弃；④ 写路径静默点收口（§11.4）；⑤ 删除用户 `$pipeline` 直通与 `store.aggregate()`，clippy 清零。

## 一、总评

| 总分 | 等级 | 结论 |
|------|------|------|
| **88.0**/100 | **A 优秀** | 可合并；4 个 Major 建议本次修复（`$nor` 空组护栏遗漏、`query_with_count` 的 `$not` 漏检、两处 DRY 复制），Minor/Info 排期跟进 |

**BLOCKED**：无（未命中一票否决清单任一条；注入面全部参数化 / 标识符引号化，无硬编码凭证、无越权可利用 IDOR）

## 二、评分卡

| # | 维度 | 满分 | 得分 | 得分率 | 等级 |
|---|------|------|------|--------|------|
| 1 | 功能正确性 | 15 | 10.0 | 67% | 及格 |
| 2 | 可靠性 | 10 | 9.0 | 90% | 优 |
| 3 | 安全性 | 15 | 15.0 | 100% | 优 |
| 4 | 性能效率 | 10 | 10.0 | 100% | 优 |
| 5 | 可维护性 | 15 | 8.0 | 53% | 差 |
| 6 | 可读性与规范 | 10 | 9.0 | 90% | 优 |
| 7 | 测试质量 | 10 | 9.5 | 95% | 优 |
| 8 | 文档 | 5 | 4.0 | 80% | 良 |
| 9 | 架构与设计 | 10 | 9.0 | 90% | 优 |
| — | 小计 | 100 | 83.5 | 83.5% | — |
| + | 亮点加分 | +5 | +4.5 | — | （逐条见"四、亮点"） |
| — | **总分** | 100 | **88.0** | — | **A 优秀** |

## 三、问题清单（按严重度）

### B Blocker（阻断 / 否决）

无。

### C Critical

无。

### M Major

| 编号 | 定位 | 问题 | 标准出处 | 修复建议 | 状态 |
|------|------|------|----------|----------|------|
| M-1 | `rust-store/core/src/types.rs:75` | 新增「空逻辑组拒绝」护栏只覆盖 `$and`/`$or`，**漏 `$nor`**：`{$nor: []}` 规划期放行 → Mongo 侧 `$nor:[]` 匹配全部文档、**静默返回全表**（SQL 侧反而在 `dialect/filter/mod.rs:179` 报错），跨后端行为不一致。同文件注释（:62）与 `validate_condition_shape`（:118）、`validate_having`（group.rs:200）均已含 `$nor`，此处为唯一遗漏 | CWE-1284（不当输入验证）/ CWE-20；项目 A-20 / G-06「绝不静默」 | 将判定改为 `matches!(k.as_str(), "$and" \| "$or" \| "$nor")`；并补 `guards.rs::empty_logical_group_is_error` 对 `$nor` 的用例（现用例仅遍历 `["$and","$or"]`，见 `guards.rs:364`，正是漏检原因） | 待修复 |
| M-2 | `rust-store/core/src/command/count.rs:126` | `has_relation_predicate` 递归只认 `$and`/`$or`/`$nor`，**未进 `$not`**：`{"$not": {"<rel>": {…}}}`（§9.6 anti-join 合法形态）不被识别 → `query_with_count` 越过守卫，把关系谓词原样交给 `countDocuments` → Mongo 当作「字段等于对象」→ **total 与 items 静默不一致**（同文件 :98 注释正是要防此情形） | CWE-391（未检查错误条件）；项目 D2 / §11.4 | 在 `has_relation_predicate` 中增加对 `$not` 的递归（`val.as_object()` 下钻）；或复用 `relation_filter` 的抽取逻辑单一实现 | 待修复 |
| M-3 | `rust-store/core/src/dialect/select/group_agg.rs:155-167` 与 `relation_agg.rs:158-170` | `count_field_pattern`（识别 `{"$cond":[{"$eq":[{"$ifNull":["$f",null]},null]},0,1]}`）**逐行完全重复**（各 13 行解析逻辑） | SonarQube 重复率（≥10 行含逻辑）；DRY / CISQ 可维护性 | 提取为 dialect 内部共享函数（如 `dialect::select::pattern::count_field_pattern`），两处复用 | 待修复 |
| M-4 | `rust-store/core/src/pipeline/group.rs:231-245` 与 `relation_filter.rs:523-542` | 收集 `$having`（含 `$and/$or/$nor` 嵌套）引用的 agg 别名，两处**同一递归算法重复实现**（15 / 20 行），仅「agg 别名来源」不同 | 同上（DRY） | 抽为 `fn collect_agg_refs(having, is_agg: impl Fn(&str)->bool, out)` 单一实现，`GroupSpec` 与 `&[(String,AggDef)]` 各自传谓词 | 待修复 |

### m Minor

| 编号 | 定位 | 问题 | 标准出处 | 修复建议 | 状态 |
|------|------|------|----------|----------|------|
| m-1 | `rust-store/core/src/command/write.rs:97-122` | `plan_exists`/`plan_count` 只加了 `validate_condition_shape`（其 `schema.relations.contains_key(k) → continue` 跳过关系键），**未拒绝「关系名作键」的条件**：`count(schema,{<rel>:{…}})` 在 Mongo 侧被当字段等值 → 静默错数（与 M-2 同源的 D2 收口缺口） | CWE-391；项目 D2 | 复用 `count.rs::has_relation_predicate`（含 M-2 修复）对 `plan_count`/`plan_exists` 的条件做同款守卫 | 待修复 |
| m-2 | `nodejs-store/src/executors/mongo.js:17-37` vs `py-store/src/py_store/executors/mongo.py:16-34` | `_explicitNull` / `_explicit_null` 对「**非 `$` 键 + 数组值**」处理不一致：JS 因 `typeof [] === 'object'` 会下钻数组，Python `isinstance(val, dict)` 为假、**不**下钻 → 同一条件（如 `{scalar: [{n: null}]}`）两宿主生成不同 Mongo 查询，破坏三端 parity（注释宣称"逐行同构"） | 项目三端 parity 规范；JS/Py profile 一致性 | 对齐两侧判定：Python 改为 `isinstance(val, (dict, list))` 或 JS 显式排除数组；并在两宿主注释中统一说明 | 待修复 |
| m-3 | `group.rs:138`、`relation_filter.rs:176/183/189/370`、`group_agg.rs:117`、`relation_agg.rs:75/116`、`schema/registry.rs:346` | 9 处 `unwrap()`/`expect()`（均在「长度==1 / position 命中」的不变量守卫之后，**panic 实际不可达**），但违反 Rust profile「非测试代码禁用 unwrap/expect」 | Rust profile（panic 面控制） | 用 `let ... else` / `if let` / `.ok_or_else()` 消解「先判定后强取」的两段式；或至少在强取处注释不变量来源 | 待修复 |
| m-4 | `rust-store/core/src/dialect/select/relation_agg.rs:228-230` | `having` 为空时**兜底** `"1 = 1"`（注释称"不可能"）：一旦上游不变量破裂，`EXISTS(… HAVING 1=1)` 恒真 → 静默返回「有子行的全部父行」，属"以兜底掩盖设计边界" | 项目 fail-loud / D2；维度 2.1 | 改为 `return Err("关系聚合谓词 having 为空")`，让不变量破裂显式失败 | 待修复 |
| m-5 | `group.rs:248`、`dialect/select/aggregate.rs:152`、`group_agg.rs:189` | 3 处 `#[allow(clippy::too_many_arguments)]` **无理由注释**（`relation_agg.rs:176` 同款 allow 却写了充分理由，应统一） | Rust profile（`#[allow]` 需理由） | 补一行理由（如「参数对齐 JS API / 跨模块传递成本」），与 `exists_clause` 保持一致 | 待修复 |
| m-6 | `group.rs:249-354`（`build_stages` 8 参 / ~106 行）、`group_agg.rs:190-369`（`translate_group` 11 参）、`aggregate.rs`（`collect_lookup` 8 参）、`relation_filter.rs:219`（`build_pred` 6 参） | 4 个函数参数 >4（阈值 ≤4） | SonarSource / Clean Code（维度 5.3） | 参数打包为 `struct GroupBuildCtx` / `JoinCtx` 等；或接受现状并在报告/注释中标注为「parity 优先」的显式取舍 | 待修复 |
| m-7 | `relation_filter.rs`（605 行）、`group.rs`（469 行） | 单文件 >400 行（参考阈值） | SonarSource 文件规模（Info~Minor） | 按职责拆分（如 relation_filter 的 parse / build / permission 三段分文件） | 待修复 |
| m-8 | `rust-store/README.md`（无 `$group`/`$having`/关系聚合谓词字样）、仓库无 `CHANGELOG.md` | rust-store 侧新能力无变更说明/CHANGELOG（py/node 两仓均有 `Unreleased` 段，rust 侧破坏性变更仅见 README 删行与 fix-plan 文档） | Conventional/工程文档实践（维度 8.3） | 补 `rust-store/CHANGELOG.md`（或在 README 增「能力矩阵」），至少覆盖 `lookup→agg`、`$pipeline`/`aggregate` 移除 | 待修复 |
| m-9 | `rust-store/doc/fix-plan/2026-09/RBAC-R0拍板与落地范围.md:7` | 文档称「代码范围：**仅 `rust-store/core`**（`core-node`/`core-py` 为薄绑定，无需改动）」，但本次工作区实际改动了 `core-node/src/{lib,methods/plan/mod}.rs`、`core-py/src/{lib,methods/plan/mod}.rs`（移除 `plan_aggregate`/`set_allow_user_pipeline`）→ 范围陈述与事实矛盾 | 文档准确性（维度 8.3） | 修正为「RBAC 判定仅落 core；同批 `$pipeline` 移除连带修改双绑定」，或注明两条变更线的边界 | 待修复 |
| m-10 | `nodejs-store/tests/**`、`py-store/tests/**` | 新增宿主逻辑（`_explicitNull`/`_explicit_null` 三态改写、`mutation_degraded` 反馈发射）**无对应宿主测试**（grep 全库无 `_explicitNull`/`relationSkipped` 用例） | ISTQB 变更配套（维度 7.1） | 补最小用例：mock Mongo 校验 `{f:null}` → `{$eq:null,$exists:true}`；`plan.degraded` 非空时 sink 收到 `mutation_degraded` | 待修复 |
| m-11 | 跨文件：`group.rs:22`（`AGG_OPS`）、`relation_filter.rs:39`（`SIMPLE_AGG_OPS`）、`relation_filter.rs:36`（`CMP_OPS`）、`schema/registry.rs:parse_agg`（`OPS`） | 同一聚合算子白名单 `$count/$sum/$avg/$min/$max` 在 4 处以字面量数组重复定义 | Clean Code 常量提取（维度 5.2/5.4） | 汇总为单一 `const AGG_OPS`（跨模块 `pub(crate)` 复用） | 待修复 |

### I Info

| 编号 | 定位 | 问题 | 标准出处 | 修复建议 | 状态 |
|------|------|------|----------|----------|------|
| I-1 | `py-store/.out/_dbg_plan.py`（被修改） | 调试脚本 `.out/_dbg_plan.py` 出现在本次 diff（调试残留物进版本库） | 维度 5.4 死代码/调试痕迹 | `.gitignore` 忽略 `.out/` 或删除该调试脚本 | 待修复 |
| I-2 | `rust-store/core/src/dialect/row.rs`（`buckets.iter_mut().find`） | 根分桶用线性 `find`，最坏 O(行数 × 根数)；大结果集可哈希 | 维度 4.1（预存，本次仅触碰未改算法） | 改用 `HashMap` + 顺序索引（`restore_rows` 内已是类似改造思路） | 待修复 |
| I-3 | `rust-store/doc/test-eval/2026/09/未处理-rust-store测试评测报告.md:97` 等既有文档 | 历史文档仍引用已删除的 `setAllowUserPipeline` 用例/函数 | 维度 8 文档一致性 | 顺手标注「已随 `$pipeline` 移除」，非本次强制 | 待修复 |

### 范围外发现

- `nodejs-store/src/executors/mongo.js` 的 `insertMany(upsertById)` 对 `docs` 逐条 `replaceOne`（N+1 写），Python 侧同构 —— 属既有设计（归档幂等），本次仅随文件抽取移动，不计分；批量插入建议评估 `bulkWrite`。
- `example/course-platform/.out/*.json`、`.out/*.json` 等**生成产物**被纳入版本库（本次 diff 数百行波动），属仓库既有约定，不计分；建议评估改由测试运行时生成。
- `py-store`/`nodejs-store` 「无 `$pipeline`、`store.aggregate()` 移除」是破坏性变更，但 `py-store/tests/test_py_store.py`、`nodejs-store/tests/test-nodejs-store.js` 仅删减对应用例，未新增「移除后仍报错」的负向锁定用例 —— 建议后续补 `assertRaises` 锁定，本次不重复计分。

## 四、亮点

- **+1.0 权限收口（RBAC R0）系统化落地**：新增单点判定 `command/query.rs::check_readable_relations`（关系可读 ∧ 目标 model 可读），单库（`build_plan`）与联邦（`plan_federated` 在剥离跨源关系**之前**校验完整 AST）共用，越权统一 `Err(ERR_PERMISSION)`；`field.read`/`relation.read` 底层工具 `permission.rs::is_field_readable/is_relation_readable` 抽取得当，`guards.rs` 新增 17 条权限用例并锁定「单库/联邦同码同文案」。证据：`command/query.rs`（`check_readable_relations`）、`federation/plan/mod.rs`、`core/tests/guards.rs`。
- **+1.0 D2「绝不静默」成体系收口**：读路径 object/array 显式投影 → `Err`（`dialect/select/mod.rs::check_projection_supported`）；写路径 `$set`/`$inc` 命中 object/array → `Err`（`dialect/write/update.rs`）；未知 `rel_type` → `Err`（`command/mutation.rs`）；SQL 无法翻译的 `$unwind`/`$addFields`/`$facet` 仅在确属已解析产物时 no-op、否则 `Err`（`dialect/select/aggregate.rs`）。
- **+1.0 归一化下推的算法难点处理正确**：§9.6 关系聚合谓词用「哨兵代理键（`__rp…​.0` + `$exists`）」把 semi/anti-join 归一为不扇出的父行形状，SQL 侧翻 `EXISTS`/`NOT EXISTS`；每父 top-N 用 `ROW_NUMBER() OVER (PARTITION BY fk ORDER BY …)` 派生表下推；全表单组空集用 `$facet`+`$replaceRoot` 对齐 SQL 无 `GROUP BY` 的「空输入 1 行」语义（`pipeline/group.rs:320-341` 注释准确）。证据：`relation_filter.rs`、`group_agg.rs`、`relation_agg.rs`、`aggregate.rs`。
- **+0.5 统一反馈通道复用**：写路径 `degraded:relationSkipped` 与联邦 `degraded` 共用既有 `feedback.emit`（node `crud/mutation.js:20`、py `crud/mutation.py:17`），符合「允许拦截，禁止静默失守」。
- **+0.5 原生边界三态对齐**：`_explicitNull` 把 `{f:null}` 编译为 `{$eq:null,$exists:true}`，与 SQL `IS NULL` 对齐，并保持幂等（二次应用不变形）。证据：`nodejs-store/src/executors/mongo.js`、`py-store/src/py_store/executors/mongo.py`。
- **+0.5 文档与工程纪律**：py/node 两仓 `CHANGELOG` 的 `Unreleased` 段把 Breaking / New / Migration 分列，迁移指引明确；三仓新增 `.trae/skills/*/SKILL.md` 准确标注「已移除能力」（`$pipeline`/`aggregate`/`setAllowUserPipeline` 现状与源码一致）；`clippy` 零警告、`eslint` 通过。

（亮点合计 +4.5，未触及封顶。）

## 五、需运行验证项

| 项 | 验证步骤 | 验证结果（复评时填） |
|----|----------|---------------------|
| Rust 单测/回归全绿 | `cargo test -p rust-store-core --no-fail-fast`（含 `guards.rs` 新增 17 权限用例、`parity_dialect.rs` +326 行） | |
| clippy 零警告 | `cargo clippy -p rust-store-core --all-targets -- -D warnings` | |
| 三端 parity（对拍） | `cargo test -p rust-store-core` + 在 `core-node`/`core-py` 侧跑 `parity`（`core-node/test/parity.test.js`、`core-py/test/parity_test.py`） | |
| ESLint | `npm run lint`（nodejs-store） | 已运行：通过（无输出） |
| Python 无 lint | 项目未配 lint —— 无门禁（建议引入 ruff/flake8，列改进建议） | |
| 真实后端 e2e（top-N 窗口 / EXISTS） | `python -m pytest tests/test_real_backends_e2e.py`（py-store）、`node --test tests/real-backends-e2e.test.js`（nodejs-store），需 MySQL/PG/SQLite/Mongo | |
| 索引命中（半连接 / 窗口） | 对 `EXISTS (...)` 与 `ROW_NUMBER() ... PARTITION BY fk` 执行 `EXPLAIN`，确认 `fk` 用上索引 | |
| `$group` 空输入对齐 | Mongo 与 SQL 各跑「by 省略 + 无匹配行」，断言两后端均返回 1 行（`$count`→0，其余→null） | |
| 行/分支覆盖率 | `cargo llvm-cov`（rust）、`pytest --cov`（py）、`c8`（node） | |

## 六、改进建议（按优先级排序）

1. **本次必须**（合并前）：修复 M-1（`$nor` 空组，与安全边界直接相关）、M-2（`$not` 漏检导致静默错 total）、M-3/M-4（DRY 复制）；补 `guards.rs` 的 `$nor` 用例。
2. **短期跟进**（1-2 迭代）：m-1（count/exists 关系键守卫）、m-3（消解守卫式 unwrap/expect）、m-4（`"1 = 1"` 兜底改 `Err`）、m-10（宿主 `_explicitNull`/`degraded` 用例）、m-2（JS/Py 边界改写对齐）、m-5/m-6/m-7（注释理由 / 参数打包 / 拆文件）。
3. **长期规划**：把「SQL 方言反向识别 core 产出的 Mongo 阶段形状」的隐式契约显式化（M-3/M-7 的根因，见架构项）；`rust-store` 纳入 CHANGELOG 惯例；py-store 引入 lint 门禁。

## 七、下一轮计划（有未闭环问题时）

- 待修复项：M-1 / M-2 / M-3 / M-4（本次必须），m-1 ~ m-11。
- 复评触发：上述修复完成后，在同一文件追加「第 2 轮评测」章节，输出维度级 delta 并逐条核对处置结果。

## 八、评分明细（逐维度扣分记录）

### 维度 1：功能正确性（15 分，实得 10.0）

```
[M] core/src/types.rs:75 → 空逻辑组护栏漏 $nor，{$nor:[]} 在 Mongo 静默全表、SQL 报错，跨后端不一致 → CWE-1284 / 项目 A-20·G-06 → -2
[M] core/src/command/count.rs:126 → has_relation_predicate 未递归 $not，$not 包裹的关系谓词越守卫 → total 静默错数 → CWE-391 / D2 → -2
[m] core/src/command/write.rs:97-122 → plan_count/plan_exists 未拒绝「关系名作键」条件 → Mongo 静默错数 → CWE-391 / D2 → -0.5
[m] nodejs-store/src/executors/mongo.js:17-37 vs py-store/.../mongo.py:16-34 → 非 $ 键+数组值的下钻行为不一致 → 三端 parity 破 → -0.5
```
核查项（未发现其他扣分）：GQL `$group` 执行序（WHERE→GROUP BY→HAVING→ORDER BY→LIMIT）与需求一致；`by`/`agg`/`having` 键域校验完备（关系/数组/裸对象/schema 外/重复键 → Err）；`acc_expr` 的 `$count:"*"`→`{$sum:1}` 与「非空计数」形态正确；全表单组空集护栏（`$facet`+`$replaceRoot`）与 SQL 天然 1 行对齐；`$not`/`$exists:false` → anti-join 否定语义正确；`restore_rows` 三态（缺失 vs 显式 null）与 `always` 写入正确；`$inc`/`$set`/`$unset` 的 `__present` 维护与 PG `EXCLUDED` 歧义修复正确。

### 维度 2：可靠性（10 分，实得 9.0）

```
[m] group.rs:138 / relation_filter.rs:176,183,189,370 / group_agg.rs:117 / relation_agg.rs:75,116 / registry.rs:346 → 9 处守卫式 unwrap/expect（panic 不可达）→ Rust profile → -0.5
[m] dialect/select/relation_agg.rs:228-230 → having 空时兜底 "1 = 1"（掩盖设计边界，应 Err）→ fail-loud/D2 → -0.5
```
核查项（未发现其他扣分）：错误一律 `Result<_, String>` 向上传播，无吞错；`$facet`/`$replaceRoot`/`$unwind`/`$addFields`/未知阶段的「命中即 Err」硬化到位；`PushdownUnsupportedError` 检查顺序修正（先于执行器检查）；SQL 不支持下推时的失败语义明确。

### 维度 3：安全性（15 分，实得 15.0）

核查项（未发现扣分项）：
- SQL/NoSQL/命令注入：全部条件值走 `build_filter` 参数化、标识符一律 `quote_ident`/`qualified_table`，未见字符串拼接外部输入（`exists_clause`、`limit_offset_sql`、`child_order_sql` 均为常量/引号化标识符）。
- 授权：`check_readable_relations` 覆盖「关系可读 ∧ 目标 model 可读」；关系聚合谓词 F2/F3 校验子字段 `field.read`；计算列 agg 依赖不可读 → `Err`；越权统一 `ERR_PERMISSION`（Host 映射 403）。
- 默认拒绝：`is_field_readable` 对未声明字段 fail-open 属 R0-4 显式决策（文档化），非缺陷。
- 敏感数据：无硬编码凭证/密钥；无日志打印敏感值。
- 输入验证：`validate_condition`（拒绝名单）、`validate_condition_shape`（U1~U4）、`validate_sort_shape`（U4）读/写两路径同码。
- 无 `unsafe` 新增；无路径遍历/反序列化/SSRF 面。
> 跨维度说明：M-1（`$nor` 全表）亦具数据暴露风险，已在主维度（功能正确性）全额扣分，此处记问题不重复扣。

### 维度 4：性能效率（10 分，实得 10.0）

核查项（未发现扣分项）：
- 下推优先：关系 JOIN / 派生表聚合 / 每父 top-N 窗口均在 DB 内完成，无 N+1；计算列 agg 走 `LEFT JOIN (… GROUP BY fk)` 单次物化。
- 只取所需字段：`requested_computes`/`projection_fields` 过滤，按需发射 `$lookup`/派生表。
- 批量：`to_list(length=None)`/`toArray()` 为既有约定；`upsertById` 逐条为既有设计（范围外）。
- 需运行验证项（不静态扣分）：真实响应时间、`EXPLAIN` 索引命中、大数据集内存水位（见「五、需运行验证项」）。

### 维度 5：可维护性（15 分，实得 8.0）

```
[M] group_agg.rs:155-167 与 relation_agg.rs:158-170 → count_field_pattern 逐行重复（13 行）→ SonarQube/DRY → -2
[M] group.rs:231-245 与 relation_filter.rs:523-542 → 收集 having agg 别名的递归逻辑重复实现 → DRY → -2
[m] AGG_OPS/SIMPLE_AGG_OPS/CMP_OPS/registry OPS → 聚合算子白名单字面量 4 处重复 → -0.5
[m] build_stages(8参)/translate_group(11参)/collect_lookup(8参)/build_pred(6参) → 参数>4 → SonarSource → -2
[m] group.rs:249-354 build_stages ~106 行（50-150）→ Clean Code 函数规模 → -0.5
```
核查项（未发现其他扣分）：模块职责清晰（group / relation_filter / group_agg / relation_agg 各司其职）；无循环依赖；命名表意良好；死代码清理彻底（`custom_pipeline_branch`、`build_compute_lookup_stages`、`build_add_fields`、`build_pipeline_projection`、`find_stage_idx`、`validate_pipeline_stages` 及其常量均已删除，全库 grep 无残引用）。

### 维度 6：可读性与规范（10 分，实得 9.0）

```
[m] group.rs:248 / aggregate.rs:152 / group_agg.rs:189 → #[allow(clippy::too_many_arguments)] 无理由（relation_agg.rs:176 有理由，不一致）→ Rust profile → -0.5
[m] relation_filter.rs:605 行 / group.rs:469 行 → 单文件 >400 行 → SonarSource（Info~Minor）→ -0.5
```
核查项（未发现其他扣分）：`cargo clippy` 零警告、`eslint` 通过、格式由 rustfmt/prettier 保证；命名见名知义（`degraded`/`proxy_key`/`count_field_pattern` 等）；注释解释「为什么」（如 `$facet` 空集对齐、`_explicitNull` 三态理由）而非复述代码；中文注释与项目既有风格一致。

### 维度 7：测试质量（10 分，实得 9.5）

```
[m] nodejs-store/tests/**、py-store/tests/** → 新增宿主逻辑（_explicitNull、mutation_degraded 发射）无对应测试 → ISTQB 变更配套 → -0.5
```
核查项（未发现其他扣分）：core `guards.rs` 新增 17 条权限用例 + U1~U4/空逻辑组用例，`parity_dialect.rs` +326 行，`fixtures/{pipeline,cases,expected}` 同步扩充，断言具体（错误码/文案/结果集），无伪测试、无 `skip` 堆积；测试命名表意、AAA 结构清晰。
> 覆盖缺口说明：`guards.rs::empty_logical_group_is_error` 仅遍历 `["$and","$or"]`（`guards.rs:364`），正是 M-1 漏检的直接原因，已在 M-1 修复建议中一并要求补用例。

### 维度 8：文档与可理解性（5 分，实得 4.0）

```
[m] rust-store 无 CHANGELOG.md；README 未记录 $group/$having/关系聚合谓词 → 变更不可追溯 → -0.5
[m] doc/fix-plan/2026-09/RBAC-R0拍板与落地范围.md:7 → 称「仅 core、绑定无需改动」与本次实际改 core-node/core-py 矛盾 → -0.5
```
核查项（未发现其他扣分）：py/node `CHANGELOG` 的 Breaking / New / Migration 三段完整且迁移指引可执行；`RBAC-R0` 文档给出决策表、落地清单（含用例名）、未落地清单、后续顺序、回归结论，质量突出；三仓 `.trae/skills/*/SKILL.md` 对「已移除能力」描述与源码一致。

### 维度 9：架构与设计（10 分，实得 9.0）

```
[m/M] dialect/select/group_agg.rs、relation_agg.rs 反向识别 core 产出的 Mongo 阶段形状（count_field_pattern 匹配 $cond/$ifNull）→ 跨层隐式契约 + 重复实现 → DIP/边界防腐 → -1
```
核查项（未发现其他扣分）：分层清晰（core 纯逻辑无 IO / 绑定薄 / Host 薄，符合铁律）；新增 `REL_PRED_PREFIX` 作为跨层约定的显式常量（好）；`PlanOut.degraded` 结构化降级声明；删除 `$pipeline` 逃生舱消除了「Mongo 专用旁路」，降低后端耦合（对齐 D18）。

## 附录

- 标准依据版本：OWASP Top 10 (2021)、MITRE CWE Top 25 (2024)、ISO/IEC 25010:2011、SonarSource Quality Model 阈值、Clean Code、ISTQB、Rust/JS/Python 语言 profile。
- 项目规则优先项：
  - `rust-store`：`cargo clippy`（本变更已清零）优先于通用 Rust 建议；
  - `nodejs-store`：ESLint（本变更通过）优先于 Standard JS 基线；
  - `py-store`：**无 lint 配置**，按 PEP 8 通用基线评（本变更未引入 PEP 8 扣分项）；
  - 项目红线：D2「绝不静默」、D18「砍 `$pipeline` 逃生舱」、三端 parity —— 本次多数问题均以此为准绳裁定。
- 本轮范围界定与裁定说明：
  - 只对「本次变更引入 / 触碰」的问题计分；历史文档残留引用、既有生成产物入库、既有 N+1 写入列「范围外发现」。
  - 守卫式 `unwrap/expect`（panic 不可达）按 Rust profile 记 Minor 而非逐处 Major，理由：均在显式长度/位置不变量守卫之后，评测重心（编译器管不到的语义错误）不在此；已在 m-3 给出消解建议。
  - DRY 计两处 Major（`count_field_pattern`、having-agg-refs 递归），均 ≥10 行且含解析逻辑，符合 scoring-rules §5.2「每处实质性重复 -2」。
- 计分自查：维度分合计 83.5，亮点 +4.5，总分 88.0（= 83.5 + 4.5，封顶 100 未触及），等级 A（80-89）——算平。

---

# 第 2 轮评测（复评）

> 复评时间：2026-09-13
> 复评对象：第 1 轮 4 个 Major + 11 个 Minor + 3 个 Info 的修复后工作区（三仓未提交工作区；本轮无中间 commit，修复直接落在工作区）
> 复评口径：与第 1 轮一致（生产系统默认权重，无降权；仅本次变更引入/触碰的问题计分；同样的一票否决清单）
> 取证：三仓 `git status --short` / `git diff --stat` + 逐文件 Read/Grep 核对；6 条验证命令实跑（结果见第五节）

## 一、复评总评

| 项目 | 第 1 轮 | 第 2 轮 | delta |
|------|---------|---------|-------|
| 维度分合计 | 83.5 | **94.5** | +11.0 |
| 亮点加分 | +4.5 | +4.5 | 0 |
| **总分** | **88.0** | **99.0** | **+11.0** |
| 等级 | A 优秀 | **S 卓越** | ↑ |
| 结论 | 可合并，4 个 Major 需本次修复 | **4 个 Major 全部闭环、6 项 Minor 闭环、6 条验证命令全绿；可合并/定稿** | — |

**BLOCKED**：无（第 1 轮即无否决项，本轮亦未命中任何一条）。

**增量要点**：第 1 轮 4 个 Major（M-1~M-4）全部已修复且实现正确，未引入新缺陷；m-1/m-2/m-4/m-5/m-9 及 I-1 共 6 项 Minor/Info 闭环。剩余 m-3/m-6/m-7/m-8/m-10/m-11 为可维护性/文档/测试配套类 Minor，I-2/I-3/I-4 为 Info，均不阻断合并。

## 二、第 1 轮问题处置核对（逐条）

| 编号 | 第 1 轮问题 | 处置 | 证据（文件:行） | 结论 |
|------|-------------|------|-----------------|------|
| M-1 | `$nor` 空逻辑组漏检 → Mongo 侧恒真静默全表 | 已修复 | `core/src/types.rs:76` 判定改为 `matches!(k.as_str(), "$and" \| "$or" \| "$nor")`；新增 `core/tests/guards.rs:836 nor_empty_logical_group_is_error` | ✅ 已修复（未引入新问题） |
| M-2 | `has_relation_predicate` 未下钻 `$not` → `query_with_count` 静默错 total | 已修复 | 上收至 `core/src/types.rs:160-181`（`$not` 递归见 :172-174）；`core/src/command/count.rs:8` 改 import、:101 调用；新增 `guards.rs:846 query_with_count_rejects_not_wrapped_relation_predicate` | ✅ 已修复（与 `pipeline/relation_filter.rs:166` 的 `$not` 语义一致） |
| M-3 | `count_field_pattern` 在 `group_agg.rs` / `relation_agg.rs` 逐行重复 | 已修复 | 单一实现上收至 `core/src/dialect/select/mod.rs:137`；`group_agg.rs:17`、`relation_agg.rs:17` 均改 import，无重复定义（全库仅 1 处 `fn count_field_pattern`） | ✅ 已修复 |
| M-4 | collecting `$having` agg 别名的递归在两处重复 | 已修复 | 单一实现上收至 `core/src/pipeline/util.rs:18`；`group.rs:19/:278`、`relation_filter.rs:28/:303` 均改 import，全库仅 1 处 `fn collect_having_agg_refs` | ✅ 已修复 |
| m-1 | `plan_exists`/`plan_count` 未拒「关系名作键」条件 | 已修复 | `core/src/command/write.rs:103-111`（`plan_exists` 守卫）、`:131-141`（`plan_count` 守卫），复用上收后的 `has_relation_predicate`；新增 `guards.rs:861 scalar_count_and_exists_reject_relation_predicate` | ✅ 已修复 |
| m-2 | 宿主 `_explicitNull`/`_explicit_null` 对「非 `$` 键 + 数组值」下钻不一致 | 已修复 | `py-store/src/py_store/executors/mongo.py:21,25,28` 改为 `isinstance(val, (dict, list))`，与 `nodejs-store/src/executors/mongo.js:22,26` 逐分支对齐（JSON 值域仅 dict/list，已等价） | ✅ 已修复 |
| m-3 | 9 处守卫式 `unwrap()`/`expect()` | 未修复 | `group.rs:138`、`relation_filter.rs:176/183/189/371`、`group_agg.rs:117`、`relation_agg.rs:75/116`、`registry.rs:346` 全部仍在；另发现第 1 轮漏检的同类项 `pipeline/relation_filter.rs:386 unreachable!("SIMPLE_AGG_OPS 已限定")` | ❌ 未修复（同类，量级未变） |
| m-4 | `relation_agg.rs` having 为空时兜底恒真 `"1 = 1"` | 已修复 | `core/src/dialect/select/relation_agg.rs:214-219` 改为 `return Err("…having 未翻译出任何条件…")`；全库已无 `1 = 1` 残字面量 | ✅ 已修复（fail-loud 到位） |
| m-5 | 3 处 `#[allow(clippy::too_many_arguments)]` 无理由 | 已修复 | `pipeline/group.rs:231`、`dialect/select/aggregate.rs:152`、`dialect/select/group_agg.rs:173` 均补中文理由，与 `relation_agg.rs:157-158` 风格统一 | ✅ 已修复 |
| m-6 | 4 个函数参数 >4（build_stages 8 / translate_group 11 / collect_lookup 8 / build_pred 6） | 未修复 | `group.rs` build_stages、`group_agg.rs` translate_group、`aggregate.rs` collect_lookup、`relation_filter.rs` build_pred 参数个数未变 | ❌ 未修复（已由 m-5 补理由为「parity/传递成本」显式取舍） |
| m-7 | 单文件 >400 行 | 未修复 | `pipeline/relation_filter.rs` 584 行、`pipeline/group.rs` 453 行、`dialect/select/aggregate.rs` 771 行 | ❌ 未修复 |
| m-8 | rust-store 无 `CHANGELOG.md`、README 未记录新能力 | 未修复 | 仓库内无 `rust-store/CHANGELOG.md`（Glob 无结果），README 未新增能力矩阵 | ❌ 未修复 |
| m-9 | RBAC 文档「仅 core、绑定无需改动」与实际矛盾 | 已修复 | `doc/fix-plan/2026-09/RBAC-R0拍板与落地范围.md:7-8` 改为「RBAC 判定逻辑仅落在 `rust-store/core`…本专题未因其改动」并注明 P5 另触碰绑定，范围陈述与事实一致 | ✅ 已修复 |
| m-10 | 宿主 `_explicitNull` / `mutation_degraded` 无对应测试 | 未修复 | `nodejs-store/tests/**`、`py-store/tests/**` grep 无 `_explicitNull`/`_explicit_null`/`relationSkipped`/`mutation_degraded` 用例（仅既有 `federation_degraded`） | ❌ 未修复 |
| m-11 | 聚合算子白名单 4 处字面量重复 | 未修复 | `pipeline/group.rs:22 AGG_OPS`、`pipeline/relation_filter.rs:36 CMP_OPS`/`:39 SIMPLE_AGG_OPS`、`schema/registry.rs:347 OPS` 仍各为字面量数组 | ❌ 未修复 |
| I-1 | 调试脚本 `py-store/.out/_dbg_plan.py` 进 diff | 已修复 | `py-store` `git status --short` 已无该文件（`git checkout` 还原生效） | ✅ 已修复 |
| I-2 | `dialect/row.rs` 根分桶线性 `find` | 未修复 | `core/src/dialect/row.rs:21` 仍 `buckets.iter_mut().find(...)` | ❌ 未修复（Info，非强制） |
| I-3 | 历史 test-eval 文档残留 `setAllowUserPipeline` 引用 | 未修复 | `doc/test-eval/2026/09/未处理-rust-store测试评测报告.md:97` 仍提 `setAllowUserPipeline` | ❌ 未修复（Info，非强制） |

**附：本轮另核实的 clippy 清零修复（均落地，未见语义副作用）**：`schema/registry.rs:11` 改 `#[derive(Debug, Clone, Default)]`；`pipeline/relation_filter.rs:258` 抽 `type ParsedRelPredicate`（type_complexity）；`computes/inject.rs:84` 改 `v.get("inject").or(Some(v))`；`dialect/write/insert.rs` Postgres/MySQL 冲突子句拆分为不同分支（identical-blocks 消除）。

## 三、第 2 轮评分卡（含 delta）

| # | 维度 | 满分 | 第 1 轮 | 第 2 轮 | delta | 第 2 轮得分率 |
|---|------|------|---------|---------|-------|----------------|
| 1 | 功能正确性 | 15 | 10.0 | **15.0** | +5.0 | 100% 优 |
| 2 | 可靠性 | 10 | 9.0 | **9.5** | +0.5 | 95% 优 |
| 3 | 安全性 | 15 | 15.0 | 15.0 | 0 | 100% 优 |
| 4 | 性能效率 | 10 | 10.0 | 10.0 | 0 | 100% 优 |
| 5 | 可维护性 | 15 | 8.0 | **12.0** | +4.0 | 80% 良 |
| 6 | 可读性与规范 | 10 | 9.0 | **9.5** | +0.5 | 95% 优 |
| 7 | 测试质量 | 10 | 9.5 | 9.5 | 0 | 95% 优 |
| 8 | 文档 | 5 | 4.0 | **4.5** | +0.5 | 90% 优 |
| 9 | 架构与设计 | 10 | 9.0 | **9.5** | +0.5 | 95% 优 |
| — | 小计 | 100 | 83.5 | **94.5** | +11.0 | — |
| + | 亮点加分 | +5 | +4.5 | +4.5 | 0 | — |
| — | **总分** | 100 | **88.0** | **99.0** | **+11.0** | **S 卓越** |

（第 1 轮「四、亮点」六项证据在本轮全部仍然成立且被修复进一步强化，故亮点保持 +4.5，不新增加分项。）

### 第 2 轮维度扣分明细（delta 归因）

```
维度1 功能正确性：15.0（+5.0）
  移除 [M] types.rs `$nor` 空组漏检 -2（M-1 修复）
  移除 [M] count.rs `$not` 漏检 -2（M-2 修复）
  移除 [m] write.rs plan_count/plan_exists 未拒关系谓词 -0.5（m-1 修复）
  移除 [m] 宿主 _explicitNull 数组下界不一致 -0.5（m-2 修复）
  → 本轮无剩余扣分项，满分

维度2 可靠性：9.5（+0.5）
  移除 [m] relation_agg.rs 兜底 "1 = 1" -0.5（m-4 修复）
  保留 [m] 9 处守卫式 unwrap/expect + 1 处 unreachable!（m-3 未修复）→ -0.5

维度3 安全性：15.0（0）—— 无扣分项（注入面参数化、越权统一 ERR_PERMISSION 未变）

维度4 性能效率：10.0（0）—— 无扣分项

维度5 可维护性：12.0（+4.0）
  移除 [M] count_field_pattern 逐行重复 -2（M-3 修复）
  移除 [M] having-agg-refs 递归重复 -2（M-4 修复）
  保留 [m] 4 函数参数 >4 -2（m-6 未修复）
  保留 [m] build_stages ~106 行 -0.5（未修复）
  保留 [m] 聚合算子白名单 4 处字面量 -0.5（m-11 未修复）

维度6 可读性与规范：9.5（+0.5）
  移除 [m] #[allow(too_many_arguments)] 无理由 -0.5（m-5 修复）
  保留 [m] relation_filter.rs 584 行 / group.rs 453 行 >400 -0.5（m-7 未修复）

维度7 测试质量：9.5（0）
  保留 [m] 宿主 _explicitNull / mutation_degraded 无用例 -0.5（m-10 未修复）
  （core 侧新增 6 条 guards 用例，断言具体、覆盖 M-1/M-2/m-1 修复点，有效性达标）

维度8 文档：4.5（+0.5）
  移除 [m] RBAC 文档范围陈述矛盾 -0.5（m-9 修复）
  保留 [m] rust-store 无 CHANGELOG、README 未记录新能力 -0.5（m-8 未修复）

维度9 架构与设计：9.5（+0.5）
  上一轮 -1 =「跨层隐式契约」+「重复实现」之和；重复实现已消解（M-3/M-4 去重→单点）
  保留 [m] SQL 方言反向识别 core 产出 Mongo 阶段形状的隐式契约 -0.5
```

**计分自查**：维度分合计 = 15.0+9.5+15.0+10.0+12.0+9.5+9.5+4.5+9.5 = 94.5；亮点 +4.5；总分 = 94.5 + 4.5 = **99.0**（封顶 100 未触及），等级 S（≥90）——算平。

## 四、第 2 轮新发现问题

**新增 B/C/M/m：无。** 本轮 6 条验证命令全绿，4 个 Major 的修复经逐行复核未见语义副作用，未引入新的正确性/安全/性能问题。

**新增 Info 级观察（不单独扣分，与既有同类项合并）**：

| 编号 | 定位 | 问题 | 标准出处 | 修复建议 |
|------|------|------|----------|----------|
| I-4 | `rust-store/core/src/pipeline/relation_filter.rs:386` | `_ => unreachable!("SIMPLE_AGG_OPS 已限定")` —— 库代码 panic 宏（第 1 轮漏检）。调用方 :331 已由 `SIMPLE_AGG_OPS` 限定，属内部不变量断言 | Rust profile（panic 面控制：库代码 panic 宏，「非内部不变量断言」才升级为 Major） | 与 m-3 同口径合并处理：可改为 `Err(...)` 显式失败，或保留并在注释中标注不变量来源；本项不单独计分 |

## 五、需运行验证项结果（第 1 轮表格逐项填结果）

| 项 | 验证步骤 | 第 2 轮验证结果 |
|----|----------|-----------------|
| Rust 单测/回归全绿 | `cargo test -p rust-store-core` | ✅ **102 passed / 0 failed**（13 个 test 目标；含 `guards.rs` 44、`parity_dialect.rs`、`parity_commands/write/fnfns`、`pushdown_usecases`、`regression_d_fixes` 等） |
| clippy 零警告 | `cargo clippy -p rust-store-core --all-targets --message-format short` | ✅ Finished，**零警告**（`-D warnings` 口径亦满足） |
| 三端 parity（对拍） | `cargo test`（`parity_*` 全绿）+ `core-node/test/parity.test.js` + `core-py/test/parity_test.py` | ✅ 全通过（core-node parity 7/7；core-py 8/8 含 parity） |
| ESLint | `npm run lint`（nodejs-store） | ✅ 通过（eslint 无输出） |
| Python 无 lint | 项目未配 lint —— 无门禁 | ⚠️ 仍无门禁（保持改进建议：引入 ruff/flake8） |
| core-py 宿主测试 | `python -m pytest rust-store/core-py/test -q` | ✅ **8 passed / 0 failed** |
| py-store 宿主测试 | `$env:LOCAL_CORE='1'; $env:PYTHONPATH='py-store/src'; python -m pytest py-store/tests -q` | ✅ **86 passed / 0 failed** |
| core-node 绑定测试 | `node --test test/parity.test.js test/dialect.smoke.test.js test/t2q.skill.test.js` | ✅ **21 passed / 0 failed**（parity 7 + dialect.smoke 3 + t2q.skill 11） |
| nodejs-store 全量测试 | `$env:LOCAL_CORE='1'; $env:NODE_ENV='test'; node scripts/test.js` | ✅ **80 passed / 0 failed**（6 suites） |
| 真实后端 e2e（top-N 窗口 / EXISTS） | `pytest tests/test_real_backends_e2e.py` / `node tests/real-backends-e2e.test.js`（需 MySQL/PG/SQLite/Mongo） | ⏸ 本轮未运行（环境无真实后端；第 1 轮亦未运行） |
| 索引命中（半连接 / 窗口） | 对 `EXISTS(...)` 与 `ROW_NUMBER() OVER(PARTITION BY fk)` 执行 `EXPLAIN` | ⏸ 本轮未运行 |
| `$group` 空输入对齐 | Mongo 与 SQL 各跑「by 省略 + 无匹配行」断言均 1 行 | ⏸ 本轮未运行（core 侧已有 `$facet` 护栏单测） |
| 行/分支覆盖率 | `cargo llvm-cov` / `pytest --cov` / `c8` | ⏸ 本轮未运行 |

## 六、遗留问题与改进建议（按优先级）

1. **可维护性主线（1-2 迭代）**：m-6（4 函数参数打包为 `struct Ctx`，-2）、m-3（9 处守卫式 unwrap/expect + I-4 `unreachable!` 用 `let ... else` / `ok_or_else` 消解，-0.5）、m-7（`relation_filter.rs` 584 / `group.rs` 453 / `aggregate.rs` 771 行拆文件，-0.5）、m-11（`AGG_OPS` 等白名单收敛为单一 `pub(crate) const`，-0.5）。
2. **文档与测试配套（短期）**：m-8（补 `rust-store/CHANGELOG.md` 或 README 能力矩阵，覆盖 `lookup→agg`、`$pipeline`/`aggregate` 移除，-0.5）、m-10（补宿主用例：`{f:null}` → `{$eq:null,$exists:true}`；`plan.degraded` 非空 → sink 收 `mutation_degraded`，-0.5）。
3. **Info 顺手项**：I-2（`row.rs:21` 分桶改 `HashMap`）、I-3（test-eval 文档标注 `setAllowUserPipeline` 已移除）、I-4（`unreachable!` 显式化）。
4. **长期架构**：维度 9 剩余 -0.5 的根因——「SQL 方言反向识别 core 产出的 Mongo 阶段形状」仍是隐式字符串契约；本轮 M-3 已收敛为单点（`dialect/select/mod.rs::count_field_pattern`），建议在该单点补齐契约文档/类型化，随下批次评估显式中间表示。
5. **工程基建**：py-store 引入 lint 门禁（ruff/flake8）；真实后端 e2e / 覆盖率纳入 CI（本轮未运行项清单）。

## 七、第 2 轮结论

- **4 个 Major（M-1/M-2/M-3/M-4）全部闭环**，修复实现正确且未引入新缺陷；**6 项 Minor/Info（m-1/m-2/m-4/m-5/m-9、I-1）闭环**。
- 6 条指定验证命令**全部通过**（Rust 102、core-py 8、py-store 86、core-node 21、nodejs-store 80，clippy/eslint 零告警），无失败、无阻塞。
- 无新增 B/C/M/m 问题（仅 1 条 Info 级观察 I-4，与既有 m-3 同类合并）。
- 总分 **99.0 / S 卓越**（+11.0），维度分合计 94.5 + 亮点 4.5 —— 算平。

**是否仍有必修项：无。** 第 1 轮判定「本次必须」的 M-1~M-4 已全部修复并验证；剩余 m-3/m-6/m-7/m-8/m-10/m-11 与 I-2/I-3/I-4 均为 Minor/Info 级技术债，**不阻断合并**。按 scoring-rules §8「连续两轮同级且问题收敛（无新增 C 及以上）→ 定稿」的精神，可合并并将报告前缀转为 `已完成-`；建议将本报告「六、遗留问题」转入技术债清单排期跟进。

---

# 第 3 轮评测（定向复评：Minor/Info 修复批次）

> 复评时间：2026-09-13
> 复评对象：第 2 轮遗留 m-3/I-4、m-11、m-8、m-10、I-2、I-3 的修复后工作区（三仓仍未提交；本轮无中间 commit，修复直接落在工作区）
> 复评口径：与第 1/2 轮一致（生产系统默认权重、无降权；仅对「本轮新改动引入 / 触碰」的问题计分；沿用同一票否决清单）
> 取证：三仓 `git status --short` + 逐文件 Read/Grep 核对 + 6 条指定验证命令实跑（结果见第五节）；绑定产物 `core-py/dist/rust_store_py.pyd`、`core-node/dist/rust-store-node.node` 为新一轮 core 重建，本轮未再重建

## 一、第 3 轮总评

| 项目 | 第 2 轮 | 第 3 轮 | delta |
|------|---------|---------|-------|
| 维度分合计 | 94.5 | **96.5** | +2.0 |
| 亮点加分 | +4.5 | +4.5 | 0 |
| 小计（原始和） | 99.0 | 101.0 | +2.0 |
| **总分（封顶 100）** | **99.0** | **100.0** | **+1.0** |
| 等级 | S 卓越 | **S 卓越** | — |
| 结论 | 无必修项，可定稿 | **m-3/I-4、m-11、m-8、m-10、I-2、I-3 全部闭环；6 条验证命令全绿；无新增问题；达到「无需再修」** | — |

**BLOCKED**：无（第 1/2 轮即未命中否决清单任一条，本轮亦未命中）。

**亮点保持 +4.5 不加不减**：第 1 轮六项亮点证据（RBAC 单点收口、D2 体系化、归一化下推算法、统一反馈通道、原生边界三态、文档纪律）在本轮全部仍然成立，且被本轮修复进一步强化；按保守口径不新增加分项。注：即使新增，总分亦受 scoring-rules「封顶 100」约束，不影响等级。

## 二、本轮修复处置核对（逐条）

| 编号 | 问题 | 处置 | 证据（文件:行） | 结论 |
|------|------|------|-----------------|------|
| m-3 | 9 处守卫式 `unwrap()`/`expect()`（先判定后强取） | 已修复 | `pipeline/group.rs:132-136`（`match (it.next(), it.next())`，错误文案「$group.agg \"{alias}\" 必须恰有一个算子键」逐字保留）；`pipeline/relation_filter.rs:168-176`（`$not` 双取值）、`:183-186` 与 `:190-194`（`preds.last().ok_or("关系聚合谓词未产生条件（内部不变量破裂）")?`）、`:373-377`（`of.ok_or_else(...)?`，文案「必须带 $of 字段」保留）；`dialect/select/group_agg.rs:114-118`（`parse_acc` 双取值，文案不变）；`dialect/select/relation_agg.rs:75-77`（`pipeline[group_idx].get("$group").ok_or(...)?`）、`:117-125`（`acc_to_sql` 双取值）；`schema/registry.rs:341-349`（`parse_agg` 双取值，文案「计算列 \"{key}\" 的 agg 必须且只能声明一个算子键」与第 1 轮一致） | ✅ 已修复（语义等价，见下「等价性裁定」） |
| I-4 | `relation_filter.rs:_ => unreachable!("SIMPLE_AGG_OPS 已限定")` 库代码 panic 宏 | 已修复 | `pipeline/relation_filter.rs:392-395` 改为 `_ => Err(format!("关系聚合谓词 \"{rel_name}\" 简写形式不支持的聚合算子 \"{opk}\""))`；`core/src` 全量 grep 已无 `unreachable!` | ✅ 已修复（panic 面归零） |
| m-11 | 聚合算子白名单在 4 处以字面量数组重复定义 | 已修复 | 单点定义 `core/src/types.rs:63 pub(crate) const AGG_OPS: [&str; 5] = ["$count","$sum","$avg","$min","$max"]`；`pipeline/group.rs:17`、`pipeline/relation_filter.rs:24`、`schema/registry.rs:7` 均改 `use ... AGG_OPS`，使用点 `group.rs:137`、`relation_filter.rs:335`、`registry.rs:350`；全库 grep 无第二处同义字面量数组（`relation_filter.rs:36 CMP_OPS` 为比较算子集，非本白名单，保留合理） | ✅ 已修复（单一来源） |
| m-8 | rust-store 无 `CHANGELOG.md`、README 未记录新能力 | 已修复 | 新增 `rust-store/CHANGELOG.md`（Breaking / New Features / Migration 三段，覆盖 `lookup→agg`、`$pipeline`/`plan_aggregate` 移除、RBAC R0 收口、object/array 读写显式报错）；`rust-store/README.md:36-46` 新增「GQL 能力（归一化）」表并链接 CHANGELOG | ✅ 已修复 |
| m-10 | 宿主 `_explicitNull`/`_explicit_null` 三态改写、`mutation_degraded` 发射无对应测试 | 已修复 | 新增 `py-store/tests/test_mongo_executor.py`（6 用例：三态改写 3 + `exec_mongo` 接线 2 + `mutation_degraded` 1；断言 `{f:null}`→`{$eq:None,$exists:True}`、嵌套对象/数组下钻、`$` 算子对象不改写、sink 事件 `type/code/layer`）；新增 `nodejs-store/tests/native-boundary.test.js`（4 用例，与 Python 侧同构）。实测增量 py-store 86→92、nodejs-store 80→84，与新增用例数完全吻合 | ✅ 已修复（断言具体，非伪测试） |
| I-2 | `dialect/row.rs` 根分桶线性 `find`（最坏 O(行数 × 根数)） | 已修复 | `core/src/dialect/row.rs:18-30` 改为 `HashMap<Value, usize>` 索引 + 保序 `Vec<Vec<&Value>>`；键仍为 `root_key(shape,row).unwrap_or_else(\|\| Value::from(i as u64))`，插入顺序即键首次出现顺序，桶内行序不变 | ✅ 已修复（与旧线性 `find` 分组语义等价，见「等价性裁定」） |
| I-3 | 历史 test-eval 文档残留 `setAllowUserPipeline` 引用 | 已修复 | `rust-store/doc/test-eval/2026/09/未处理-rust-store测试评测报告.md:97`（C6 行）加注「`setAllowUserPipeline` 已随归一化 P3『砍 `$pipeline` 逃生舱』移除，本条为历史快照，现状见 rust-store/CHANGELOG.md」 | ✅ 已修复 |

### 等价性裁定（重点复核三项）

1. **relation_filter 双取值 vs 原 `len()!=1` 校验**：原逻辑「`if vo.len() != 1 { Err }` 后 `.next().unwrap()`」判定的正是「迭代器恰有 1 项」；新逻辑 `match (it.next(), it.next()) { (Some(kv), None) => kv, _ => Err }` 在「恰 1 项」时返回该键、其余（0 项 / ≥2 项）一律 Err，判定域与结果完全一致。`group.rs`、`group_agg.rs::parse_acc`、`registry.rs::parse_agg`、`relation_agg.rs::acc_to_sql` 同款改写，均**逐字保留原错误文案**（仅 `relation_agg.rs:122` 在不可达分支的文案尾部加注「（内部不变量破裂）」，见第四节附注）。`preds.last().ok_or(...)?` 对应原 `.unwrap()`：`build_pred` 成功即 push 恰一条，`last()` 必为 `Some`，改写不改变成功路径，仅把「不变量破裂」由 panic 降级为可传播 Err。
2. **row.rs 分桶改写**：键推导、插入顺序、桶内顺序三者均未变，仅把「线性查找桶下标」换成「哈希索引桶下标」；`HashMap` 仅承担「键 → 下标」查表，输出仍由 `Vec` 的插入序驱动，故**分组语义与文档顺序完全保持**。复杂度由 O(行数 × 根数) 降为均摊 O(行数)。
3. **`_ => Err(...)` 取代 `unreachable!`**：调用方 `relation_filter.rs:335` 已把键域限定为 `$exists ∪ AGG_OPS`，而 match 已覆盖 `$exists/$count/$sum/$avg/$min/$max`，故 `_` 臂实际不可达；改为显式 Err 后正常路径行为不变，仅消除了库代码 panic 宏。

## 三、第 3 轮评分卡（含 vs 第 2 轮 delta）

| # | 维度 | 满分 | 第 1 轮 | 第 2 轮 | 第 3 轮 | delta(3−2) | 第 3 轮得分率 |
|---|------|------|---------|---------|---------|------------|----------------|
| 1 | 功能正确性 | 15 | 10.0 | 15.0 | **15.0** | 0 | 100% 优 |
| 2 | 可靠性 | 10 | 9.0 | 9.5 | **10.0** | +0.5 | 100% 优 |
| 3 | 安全性 | 15 | 15.0 | 15.0 | 15.0 | 0 | 100% 优 |
| 4 | 性能效率 | 10 | 10.0 | 10.0 | **10.0** | 0 | 100% 优 |
| 5 | 可维护性 | 15 | 8.0 | 12.0 | **12.5** | +0.5 | 83% 良 |
| 6 | 可读性与规范 | 10 | 9.0 | 9.5 | 9.5 | 0 | 95% 优 |
| 7 | 测试质量 | 10 | 9.5 | 9.5 | **10.0** | +0.5 | 100% 优 |
| 8 | 文档 | 5 | 4.0 | 4.5 | **5.0** | +0.5 | 100% 优 |
| 9 | 架构与设计 | 10 | 9.0 | 9.5 | 9.5 | 0 | 95% 优 |
| — | 小计 | 100 | 83.5 | 94.5 | **96.5** | +2.0 | — |
| + | 亮点加分 | +5 | +4.5 | +4.5 | +4.5 | 0 | — |
| — | **总分** | 100 | **88.0** | **99.0** | **100.0**（原始和 101.0 封顶） | **+1.0** | **S 卓越** |

### 第 3 轮维度扣分明细（delta 归因）

```
维度2 可靠性：10.0（+0.5）
  移除 [m] 9 处守卫式 unwrap/expect + 1 处 unreachable! -0.5（m-3 / I-4 修复）→ 本轮无剩余扣分项，满分

维度5 可维护性：12.5（+0.5）
  移除 [m] 聚合算子白名单 4 处字面量 -0.5（m-11 修复）
  保留 [m] 4 函数参数 >4（build_stages 8 / translate_group 11 / collect_lookup 8 / build_pred 6）→ -2（m-6 未修复）
  保留 [m] build_stages ~106 行 -0.5（未修复）

维度7 测试质量：10.0（+0.5）
  移除 [m] 宿主 _explicitNull / mutation_degraded 无用例 -0.5（m-10 修复）→ 本轮无剩余扣分项，满分

维度8 文档：5.0（+0.5）
  移除 [m] rust-store 无 CHANGELOG、README 未记录新能力 -0.5（m-8 修复）→ 本轮无剩余扣分项，满分

维度4 性能效率：10.0（0）
  I-2（row.rs 分桶）为 Info 级预存项，第 1/2 轮即未单独扣分；本轮修复后仍 10.0，不产生新 delta

维度1/3/6/9：15.0 / 15.0 / 9.5 / 9.5（均 0）
  维度6 保留 [m] 单文件 >400 行 -0.5（m-7 未修复）
  维度9 保留 [m] SQL 方言反向识别 core 产出 Mongo 阶段形状的隐式契约 -0.5（未消解）
```

**计分自查**：维度分合计 = 15.0+10.0+15.0+10.0+12.5+9.5+10.0+5.0+9.5 = 96.5；亮点 +4.5；原始和 = 96.5 + 4.5 = 101.0 → 按 scoring-rules「封顶 100」取 **100.0**，等级 S（≥90）——算平（封顶项已显式标注）。

## 四、第 3 轮新发现问题

**新增 B/C/M/m/I：无。**

- 本轮 6 条验证命令全绿；7 项修复经逐行复核**语义等价**，未发现任何正确性、可靠性、安全、性能或测试层面的新问题。
- 附注（不计分）：`dialect/select/relation_agg.rs:122` 不可达分支的错误文案，相较第 2 轮由「关系聚合谓词累积器必须恰有一个算子键」扩为同句 + 后缀「（内部不变量破裂）」。该分支仅在「规划产物被外部篡改」时可达，且全库无测试/调用方依赖该字符串，属纯提示语增强，不构成缺陷。
- 范围外发现（不计分）：`core-py/src/convert.rs:35-49` 存在 5 处 `into_pyobject(py).unwrap()`。经核实为 PyO3 对 `bool`/`i64`/`u64`/`f64`/`&str` 的**不可失败转换**，且本批次 `git status` 未触碰该文件（`convert.rs` 无改动），属历史既有代码，列范围外；若后续统一 Rust profile「非测试代码禁用 unwrap」可评估改 `expect` 或加注理由。

## 五、运行验证结果

| 项 | 命令（PowerShell） | 第 3 轮结果 |
|----|----------|-------------|
| Rust 单测/回归 | `cargo test -p rust-store-core`（cwd=rust-store） | ✅ **102 passed / 0 failed**（13 个 test 目标：7+44+1+1+1+25+2+1+1+6+10+3+0） |
| clippy 零警告 | `cargo clippy -p rust-store-core --all-targets --message-format short`（cwd=rust-store） | ✅ Finished，**零 warning / 零 error** |
| core-py 绑定测试 | `python -m pytest rust-store/core-py/test -q` | ✅ **8 passed / 0 failed** |
| py-store 宿主测试 | `$env:LOCAL_CORE='1'; $env:PYTHONPATH='py-store/src'; python -m pytest py-store/tests -q`（cwd=根） | ✅ **92 passed / 0 failed**（第 2 轮 86 + 新 `test_mongo_executor.py` 6） |
| core-node 绑定测试 | `$env:LOCAL_CORE='1'; node --test test/parity.test.js test/dialect.smoke.test.js test/t2q.skill.test.js`（cwd=rust-store/core-node） | ✅ **21 passed / 0 failed**（parity 7 + dialect.smoke 3 + t2q.skill 11） |
| nodejs-store 全量测试 | `$env:LOCAL_CORE='1'; $env:NODE_ENV='test'; node scripts/test.js`（cwd=nodejs-store） | ✅ **84 passed / 0 failed / 6 suites**（第 2 轮 80 + 新 `native-boundary.test.js` 4；`scripts/test.js` 以 `tests/**/*.js` 收编，新文件确被纳入） |
| 真实后端 e2e / `EXPLAIN` / 覆盖率 | 同第 1/2 轮（需 MySQL/PG/SQLite/Mongo） | ⏸ 本轮仍未运行（环境无真实后端；不影响本轮定向复评结论，保持改进建议项） |

## 六、仍未修复项（保留为技术债）

本轮核实后仍保留 **m-6** 与 **m-7** 两项，均属可维护性/规模类 Minor，保留理由与不阻断合并的判断如下：

| 编号 | 现状（本轮实测） | 保留理由 | 不阻断合并判断 |
|------|------------------|----------|----------------|
| m-6 参数 >4 | `pipeline/group.rs::build_stages`（8 参）、`dialect/select/group_agg.rs::translate_group`（11 参）、`dialect/select/aggregate.rs::collect_lookup`（8 参）、`pipeline/relation_filter.rs::build_pred`（6 参）参数个数未变 | 四处均为「归一化下推上下文」的显式横向传递，参数彼此强相关且无独立生命周期；第 2 轮已由 m-5 在 `#[allow(clippy::too_many_arguments)]` 处补齐中文理由（「拆结构体反而增加跨模块传递成本」）形成**显式取舍**，属 parity 优先的设计决策，非疏忽 | 不阻断：纯签名风格问题，无行为/安全/性能影响；建议随下批次「下推中间表示类型化」（维度 9 剩余 -0.5 的同一根因）一并用 `struct Ctx` 收敛，届时一并清偿 |
| m-7 单文件 >400 行 | `pipeline/relation_filter.rs` 592 行、`pipeline/group.rs` 451 行、`dialect/select/aggregate.rs` 771 行（均 >400 参考阈值） | 三个文件职责仍内聚（各自「解析 / 规划 / 翻译」单一主题），拆分会引入跨文件跳转成本；阈值 400 为 SonarSource **参考值**，项目无更严规范 | 不阻断：规模类 Minor，无圈复杂度/嵌套深度超限；建议在「方言中间表示显式化」重构中按 parse/build/emit 三段自然拆分，避免为拆而拆 |

> 说明：m-6/m-7 与第 2 轮结论一致（连续两轮保持同级别、未扩散），符合 scoring-rules §8「问题收敛」的定稿条件。

## 七、第 3 轮结论

- **第 2 轮遗留的 7 项全部闭环**：m-3、I-4（m-3 同类合并项）、m-11、m-8、m-10、I-2、I-3 逐项核实**修复正确、语义等价、未引入回归**。
- **6 条指定验证命令全部通过**（Rust 102、core-py 8、py-store 92、core-node 21、nodejs-store 84；clippy 零告警），无失败、无阻塞；新增宿主测试的通过增量与用例数完全吻合。
- **本轮新改动未引入任何新 B/C/M/m/I 问题**（仅 1 条不可达分支文案增强与 1 条范围外历史的 PyO3 `unwrap` 观察，均不计分）。
- 总分 **100.0 / S 卓越**（维度分合计 96.5 + 亮点 4.5 = 原始和 101.0，按规则封顶 100.0）；相对第 2 轮 +1.0。
- 剩余 m-6（参数打包）、m-7（单文件超长）为**可维护性/规模类技术债**，已给出保留理由，**不阻断合并**。

**是否达到「无需再修」：是。** Major 及以上问题已连续两轮归零，Minor 仅剩 m-6/m-7 两项且均已由显式取舍或参考阈值定性为技术债；按 scoring-rules §8，建议将报告前缀转为 `已完成-`，并把 m-6/m-7 转入技术债清单随下一批次「方言中间表示类型化 + 文件职责拆分」一并排期。
