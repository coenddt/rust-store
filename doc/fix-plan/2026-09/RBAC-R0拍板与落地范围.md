# RBAC · R0 拍板与落地范围（core 层）

> 记录时间：2026-09-13
> 状态：**R0 拍板完成 · 本期核心已落地**
> 上游约束：《未处理-权限RBAC设计差异清单.md》（下称「差异清单」）§6 待拍板问题；
> 《未处理-多后端归一化执行计划.md》§9.1(R6/R7)、§9.2、§9.6、§10.5/§10.6
> 代码范围：**RBAC 判定逻辑仅落在 `rust-store/core`**（`core-node` / `core-py` 为薄绑定，本专题未因其改动）。
> 注：同期「多后端归一化 P5」的变更另需同步 `core-node` / `core-py` 透出层与双宿主（`index.d.ts` / `plan/mod.rs` 等），与本专题范围无关。
> 关联文档：`rust-store/doc/code-review/2026/09/未处理-权限注入收口专题.md`（A/B 组权限缝隙，本期部分覆盖）

---

## 1. R0 决策（6 条，已拍板 · 不再变更）

| # | 决策 | 落点 |
|---|---|---|
| R0-1 | **不可读表 / 关系**：显式请求时 → **`Err(ERR_PERMISSION)`**（表级与关系级策略一致，对齐 D2「绝不静默」）；不再静默省略 / 降级。 | `command/query.rs::check_readable_relations`；`federation/plan/mod.rs`；`pipeline/relation_filter.rs` |
| R0-2 | **不可读字段**：凡被 `$condition` / `$sort` / 聚合引用 → **`Err`**。 | 聚合面已落地（F2/F3）；`$condition`/`$sort` 标量面见「§3 未落地 F4」 |
| R0-3 | **派生值**：计算列（含 `agg` 形态）的依赖字段 / 关系不可读 → **`Err`**（不因「只是派生值」而放行）。 | `pipeline/lookup.rs::build_agg_stages`；`check_readable_relations`（depends 注入关系） |
| R0-4 | **默认姿态**：**保持现状 opt-in**（`require_context` 默认 `false`，fail-open），**不改为默认 fail-secure**。生产应用应显式 `set_require_context(true)`。 | `schema/registry.rs`（未改） |
| R0-5 | **行策略 DSL**：本期**不实现**通用 `rowFilter`。 | 未落地（R1） |
| R0-6 | **关系写权限**：保持现状「read 门控写」，**不新增** `relation.write`。 | 未落地（L3） |

> 关于 R0-2 / R0-4 的边界说明：R0-2 的「聚合」面已由 F2/F3 完整落地；其「`$condition`/`$sort` 标量字段读校验」属于差异清单 **F4** 项，本期按范围约束**未实施**（见 §3）。R0-4 决定维持 fail-open，因此 `ctx = None` 时所有新增校验一律放行（与既有姿态、三端 parity 一致）。

---

## 2. 本期已落地清单（core 层）

| 差异清单编号 | 内容 | 落点（文件） | 用例 |
|---|---|---|---|
| **F2** | 根级 `$group` 的 `by` 键 / `agg` 引用字段过 `field.read`；`$having` 引用 by 键 / agg 别名背后的引用字段一并覆盖 → 越权 `Err(ERR_PERMISSION)` | `pipeline/group.rs::validate_read_permission`（由 `pipeline/build.rs` 调用） | `guards.rs::group_unreadable_by_field_is_error` / `group_unreadable_agg_field_is_error` / `group_having_backed_by_unreadable_agg_is_error` / `group_readable_fields_pass` |
| **F3** | §9.6 关系聚合谓词 `filter` / `agg` / 简写 `$of` 引用的**子字段**过子模型 `field.read` → 越权 `Err` | `pipeline/relation_filter.rs::plan` + `check_filter_readable` | `guards.rs::rel_predicate_unreadable_filter_field_is_error` / `rel_predicate_unreadable_of_field_is_error` / `rel_predicate_unreadable_relation_is_error` / `rel_predicate_readable_fields_pass` |
| **F6 / R0-3** | 计算列 `agg` 形态（`{"$sum":"lessons.duration"}`）的**依赖关系 / 目标 model / 子字段**不可读 → `Err` | `pipeline/lookup.rs::build_agg_stages` | `guards.rs::compute_agg_unreadable_relation_is_error` / `compute_agg_unreadable_child_field_is_error` / `compute_agg_readable_dependency_passes` |
| **L1 / T2** | 读路径上 GQL **显式请求的关系**必须：`relation.read` 可读 **∧** 目标 model `can_read_schema` 可读；否则 `Err`（单库 + 联邦一致） | `command/query.rs::check_readable_relations`（`build_plan` 内调用）；`federation/plan/mod.rs`（剥离跨源关系前对完整 AST 校验） | `guards.rs::explicit_unreadable_relation_is_error` / `explicit_unreadable_target_model_is_error` / `explicit_readable_relation_passes` / `relation_permission_error_identical_across_plan_paths` |
| **L2 / T3** | 不可读关系 = `Err(ERR_PERMISSION)`（同一 code + 文案），与表级策略一致，不再静默省略 | 同上 + `relation_filter.rs`（原自定义文案统一为 `ERR_PERMISSION`） | `relation_permission_error_identical_across_plan_paths`（单库/联邦同码同文案） |
| **L5（核心判定）** | 「关系可见 = 关系可读 ∧ 目标表可读」的合并判定 | `check_readable_relations` 单点实现，避免关系路径各处各写一套 | 同上 |
| **L6** | 计算列 `depends` 经 `merge_depends_into_ast` 自动注入的关系同样过 `relation.read` → 不可读 `Err` | 注入发生在 `plan_query_ast_mut` / `plan_federated`，随后由 `check_readable_relations` 统一判定 | 由 `check_readable_relations` 覆盖（与 L1 用例同组） |
| **F1** | `field.read` 投影裁剪保留（`apply_permission_prune`），覆盖面扩展至上述聚合 / 关系引用 | `pipeline/projection.rs`（未改，行为保留） | 既有 `parity_computes` / `fixtures` |
| **F5** | join key 不可读时仍可内部用于匹配，但**不得出现在输出**（沿用 `apply_permission_prune` + `process_node` 裁剪），补测试锁定 | `pipeline/projection.rs` / `computes/run/sync.rs`（未改） | `guards.rs::unreadable_join_key_not_in_output` |
| **X1** | 权限 parity：同一 ctx 下，单库 `plan_query` 与联邦 `plan_federated` 的裁剪结果（行 / 字段集）一致 | 测试向 | `guards.rs::permission_prune_parity_single_vs_federation` |

### 2.1 关键实现说明

- 新增统一判定函数 `command/query.rs::check_readable_relations(relations, schema, registry, ctx)`：
  - 递归遍历 AST 关系树；关系 `read` 不可读 **或** 目标 model `can_read_schema` 不可读 → `Err(ERR_PERMISSION)`；
  - `ctx = None` 直接放行（fail-open，R0-4）；
  - 未声明 `model` 的「关系」按普通字段跳过（与 federation `walk` 口径一致）。
- 判定挂载点：
  - 单库：`plan_query_ast_mut` → `build_plan`（在 `build_pipeline` 前，含 `depends` 注入后的关系）；
  - 联邦：`plan_federated` 在 `walk` 剥离跨源关系**之前**对完整 AST 校验（跨源关系不会被漏判），并对每个子单元经 `plan_query_ast_mut` 复核。
- 新增底层工具 `permission.rs::is_field_readable` / `is_relation_readable`：
  - 点号路径按 root 字段判定；`schema` 未声明的字段视为可读（避免误伤「未声明字段可用于过滤」的既有容忍语义）；
  - `ctx = None` → 放行。
- 越权错误统一走 `ERR_PERMISSION`（`ERR_PERMISSION:无访问权限`，稳定前缀 `ERR_PERMISSION:`），Host 按前缀映射 403 类错误；**4 库 + 双宿主同码同文案**（core 规划期统一拒绝）。

---

## 3. 明确未落地清单（本期范围外）

### 3.1 表级

- **T1** 分操作粒度（`create/read/update/delete` 拆分 `write`）：未落地。
- **R8 / X5** owner / 行策略字段纳入 `requiredIndexes`：未落地。

### 3.2 行级（R 系列）

- **R1** 通用 `rowFilter` DSL（R0-5）：未落地。
- **R2** `Context` 扩展 `tenantId` / `orgId` / `deptId` / `attrs`：未落地。
- **R3** owner 条件下推落点（`UnitSpec.permission` 契约）：未落地（现状 owner 注入仍在 `$match` / `$condition`，联邦子单元由 `merge_owner_condition` 注入）。
- **R4** `$group` / `$having` 前 owner 注入专项：未落地（现状根 owner 注入不变；聚合终结路径 `postprocess` 置空，未见越权回归用例）。
- **R5** 关系聚合谓词子单元 owner（`EXISTS` / 派生表内）：部分依赖既有 `build_lookup_stage` 内 `merge_owner_condition`，未新增专项断言。
- **R6** 递归关系 owner 注入：未落地（递归能力尚未在本期范围）。
- **R7** keyset `$after` 游标绑定 owner：未落地。

### 3.3 字段级（F 系列）

- **F4** `$condition` / `$sort` 引用的**不可读标量字段** → `Err`（R0-2 已拍板，本期未实施）：
  - 说明：本期「本期必须实现」范围限定为**聚合与关系谓词**引用收口（F2/F3）；根级 / 关系级**标量** `$condition` / `$sort` 的字段读校验会改变既有「未声明 / 未配置字段可用于过滤」的容忍语义，且需覆盖「关系路径排序（R10）」等分支，单独评估后落地。
- **F7** 嵌套 object 路径级写权限（现按 root 检查）：未落地。
- **F8** 读侧 `_id` 豁免 / 不豁免规则对齐：未落地。

### 3.4 关系级（L 系列）

- **L3** `relation.write`：未落地（R0-6 冻结「read 门控写」）。
- **L4** 别名（D17 `alias: relName`）权限判定用 schema 原名：当前 GQL 解析器尚未支持别名语法，未落地。
- **L5** 权威合并规则**完整形态**（关系可读 ∧ 目标表读 ∧ 目标行级 owner ∧ 目标字段读 四者统一构造器）：本期只落地「关系可见 = 关系可读 ∧ 目标表可读」；目标行级 owner（`build_lookup` / `build_agg_stages` 内 `merge_owner_condition`）与目标字段读（投影 / `process_node` 裁剪）沿用既有实现，未合并为单点构造器。
- **L7** 不可读关系上写谓词 → `Err`：归一化已定，本期未新增。

### 3.5 权限 × 执行路径（X 系列）与模型（M 系列）

- **X2** 三路径统一权限条件构造器（`UnitSpec.permission`）：未落地。
- **X3** CTE 物化下 owner 落点：未落地。
- **X4** 权限问题一律 fail-closed（不 `degraded`）：未落地（现状权限错误即 `Err`，但未加「禁止降级放行」的显式断言）。
- **X5** `requiredIndexes` 纳入 owner / 行策略字段：未落地。
- **X6** 关系谓词 semi-join 父子两侧权限 DB 内完成：未落地。
- **M1** 默认 `require_context = true`（fail-secure）：未落地（R0-4 维持 opt-in）。
- **M2** 内置角色（`super_admin`/`admin`/`guest`）行为可配置：未落地。
- **M3** 通用「拥有者字段」声明（去硬编码 `createdBy`）：未落地。
- **M4** 角色继承：未落地。
- **M5** schema 级策略继承：未落地。

---

## 4. 后续建议顺序

1. **F4**：落地 R0-2 的 `$condition` / `$sort` 标量字段读校验（先补「关系路径排序」分支口径，避免与 R10 冲突；同步评估「未声明字段」容忍语义）。
2. **R2 + M1**：`Context` 扩展 tenant/org（多租户隔离）+ 生产模板默认 `require_context = true`（`None` 与 `Context::system()` 语义分离），三端 parity 一并评估。
3. **X2 + X4**：定义 `UnitSpec.permission`，把权限条件收为计划契约的单点构造器，三路径共用；显式断言权限失败不进入 `degraded`。
4. **L4 + L5（完整形态）**：随 GQL 别名语法落地，统一「关系可见」四判定。
5. **T1 / L3 / R1**：分操作粒度、`relation.write`、通用 `rowFilter` 按需推进（破坏性变更，需 CHANGELOG + 三仓同步）。
6. **R3/R5/R6/R7/R8 + X3/X5/X6 + M2~M5**：随归一化下推 / 递归 / 索引契约 / 多租户批次联动。

---

## 5. 回归与验证

- `cargo test -p rust-store-core --no-fail-fast`：**全绿**（含新增 17 条权限用例）。
- `cargo check --workspace --all-targets`：**无错误 / 无警告**。
- 既有测试**无需按新决策改写期望**：原有用例中「关系不可读 = 静默省略」仅存在于 `fixtures/computes/cases.json` 的 `process_node`（终态防护）直测用例（`pn-perm-relation-prune`），其为 finalize 阶段的安全网而非 GQL 规划入口；本期的 `Err` 判定发生在**规划期**，正常流程下不可读关系在到达 `process_node` 之前即被拒绝，故该防护行为保留、用例期望不变。
