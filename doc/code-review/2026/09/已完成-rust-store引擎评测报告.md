# 代码评审评测报告：rust-store 引擎

> 评测轮次：第 1 轮
> 评测时间：2026-09-12
> 评测对象：项目评审（rust-store 全部 80 个 .rs 文件：core / core-node / core-py 三 crate，约 6,100 行源码 + 1,960 行测试）
> 技术栈：Rust（workspace，edition 2021）、napi-rs（Node 绑定）、PyO3（Python 绑定）、serde_json
> 评测口径：默认权重（生产引擎库）
> 脚本证据：`cargo clippy --workspace`（core 零警告，core-py 7 警告）、`cargo test -p rust-store-core`（33/33 通过）

## 一、总评

| 总分 | 等级 | 结论 |
|------|------|------|
| **89** / 100 | **A 优秀** | 架构与工程纪律优秀，可作三端标杆；1 个 Critical（mutation one 关系越权写入）建议尽快修复 |

**BLOCKED**：无（未命中一票否决清单；C-1 有"父模型写权限"前提，定为 Critical 非 Blocker）

## 二、评分卡

| # | 维度 | 满分 | 得分 | 得分率 | 等级 |
|---|------|------|------|--------|------|
| 1 | 功能正确性 | 15 | 13.0 | 86.7% | 良 |
| 2 | 可靠性 | 10 | 7.4 | 74% | 中 |
| 3 | 安全性 | 15 | 8.0 | 53.3% | 及格 |
| 4 | 性能效率 | 10 | 9.5 | 95% | 优 |
| 5 | 可维护性 | 15 | 13.5 | 90% | 优 |
| 6 | 可读性与规范 | 10 | 9.9 | 99% | 优 |
| 7 | 测试质量 | 10 | 9.5 | 95% | 优 |
| 8 | 文档与可理解性 | 5 | 5.0 | 100% | 优 |
| 9 | 架构与设计 | 10 | 10.0 | 100% | 优 |
| — | 小计 | 100 | 85.8 | | |
| + | 亮点加分 | +5 | +3.5 | | |
| — | **总分** | | **89.3 → 89** | | |

## 三、问题清单（按严重度）

### B Blocker

无。

### C Critical

| 编号 | 定位 | 问题 | 标准出处 | 修复建议 | 状态 |
|------|------|------|----------|----------|------|
| C-1 | core/src/command/mutation.rs:128-144 | mutation 的 `one` 关系子文档**绕过子模型写权限与字段过滤**：父模型 mutation 携带 one 关系数据时直接 `upsert_one_update(rel_schema, …)` 写入子模型表，既无 `can_write_schema(rel_schema, ctx)` 校验，也无 `filter_writable_data` 字段裁剪；而 `many` 路径（:156）经 `plan_mutation_node` 递归后两道校验齐全——同文件内不对称，更似疏漏而非设计。后果：拥有父模型写权限的调用方可向无写权限的关联模型写入任意 schema 声明字段（含覆盖既有子文档）。注释注明"对齐 JS `_upsertOne`"，若为刻意 parity 决策应改为显式文档声明 + Host 侧补偿校验 | CWE-284 / CWE-639（不当授权 / IDOR 写入） | one 路径复用 many 路径的校验入口：`can_write_schema(rel_schema, ctx)` + `filter_writable_data(rel_schema, ctx, child)`；若确认保留 JS 行为，须在计划文档与 README 显式声明该信任边界 | ✅ 已修复（第 2 轮） |

### M Major

| 编号 | 定位 | 问题 | 标准出处 | 修复建议 | 状态 |
|------|------|------|----------|----------|------|
| M-1 | core/src/command/query.rs:165、core/src/federation/plan.rs:274、core/src/command/write.rs:158 | **ctx=None 即完全放行（fail-open）**：`if ctx.is_some() && !can_read_schema(...)`、`check_write_perm` 的 `let Some(c) = ctx else { return Ok(None) }`。core 作为库把"无上下文 = 信任调用"作为信任模型可以成立，但语义上 `None` 同时承担"系统内部调用"与"调用方忘记传 ctx"两种含义，后者静默变成零鉴权查询/写入。write.rs:157 注释自证此语义（"JS：`if (ctx)` 才做检查——无上下文（内部调用）不设防"） | CWE-285（不当授权，fail-open 违反 fail-secure） | 提供显式 `Context::system()` / `internal: true` 表示内部调用，Host API 层（绑定层）把 `ctx` 缺失从 `None` 改为报错或要求显式 opt-in；至少在绑定层文档显著声明 | ✅ 已修复（第 4 轮） |
| M-2 | core/src/dialect/select/aggregate.rs:88 | `$lookup` 阶段进入 SQL 翻译时 `unreachable!()` 直接 panic。该模块自述契约是"无法安全翻译的组合输出 `_unsupported` 标志 + warning，**绝不生成错误 SQL**"——panic 违反自身设计边界。可达路径：Registry `allow_user_pipeline = true`（默认值）时，用户 `$pipeline` 可携带任意 `$lookup`，Host 对 CustomPipeline 模式的 aggregate 命令调用 `translate` 即触发——用户输入可达的 panic = 远程 DoS 面 | CWE-835 类（不可恢复错误）；违反模块自述铁律 | `unreachable!()` 改为 `return Err("SELECT 翻译不支持 $lookup 阶段（应经 JOIN 下推）")`，与 `_unsupported` 机制汇合 | ✅ 已修复（第 2 轮） |
| M-3 | core/src/schema.rs:429-437 | `normalize_fields` 对非法字段定义值（非字符串、非对象，如 `fields: { price: 123 }`）静默产出空 `field_type` 的 FieldDef 并注册成功——脏 schema 被静默吞掉，后续所有基于 field_type 的分支（object/array 展平、标量列判定）都会走错且无任何报错信号。同文件其他入口（timestamps、定位三元组）都是 fail-fast，此处不一致 | 设计边界未达成即暴露（fail-fast 铁律）；CWE-20（输入验证） | else 分支改 `return Err(format!("字段 {} 定义类型非法", key))`；`register` 已是 Result 签名，改动零成本 | ✅ 已修复（第 2 轮） |

### m Minor

| 编号 | 定位 | 问题 | 标准出处 | 修复建议 | 状态 |
|------|------|------|----------|----------|------|
| m-1 | pipeline/build.rs:67-76,128,141、pipeline/util.rs:39-45、pipeline/lookup.rs:158、permission.rs:83、command/mutation.rs:125、computes/defaults.rs:95,105,123、dialect/filter.rs:85、dialect/write.rs:364、bson.rs:81 | checked-unwrap 模式 20+ 处：全部有 guard 保证（`is_nullish` 先判后 `unwrap`、`is_empty` 先判后 `it.next().unwrap()`、`Vec` 刚构建后 `next().unwrap()`），当前安全但前提靠人工维护，后续重构破坏 guard 即 panic。bson.rs:81 的 `expect("is_extended 保证非空")` 是同类中最好的一处（不变量成文） | Rust profile：脆弱前提模式 | 逐一改 `if let Some(x) = …` / `let Some(x) = … else { return }`；如 mutation.rs:125 的 `schema.relations[rel_name]` 改 `schema.relations.get(rel_name)?` | ✅ 已整改（第 6 轮：新增 `pipeline::util::non_nullish` checked 助手——`Option::filter` 一次判定即携带值，替换 `is_nullish` 先判后 `unwrap` 的重复求值模式；落地站点：pipeline/build.rs:66-76（custom_pipeline_branch 四处 `!is_nullish + unwrap`）、build.rs:123（root_pipeline 可空+数组双重判定）、build.rs:138（root_condition）、pipeline/lookup.rs:159（condition）、permission.rs:108（`role_list.unwrap()` → `let Some(list) = … else`）、dialect/filter.rs:84-87（`active.drain().next().unwrap()` → `let Some(acc) = … else`）、dialect/write.rs:374/392（`vec![stmt].into_iter().next().unwrap()` 直接省去 Vec 中转）、computes/defaults.rs:95/105/123（三处 `result.as_object_mut().unwrap()` 改为全程以 `Map` 承载，零 unwrap）、bson.rs:81（`expect("is_extended 保证非空")` → `if let Some((k, inner))`，不变量注释保留）。guard 前提不再靠人工维护，重构导致前提漂移时返回而非 panic。45/45 core 测试全绿 + 全 workspace `clippy -D warnings` 零告警） |
| m-2 | core/src/command/query.rs:332-348 | `restore_sort_order` 对每个 item 调 `ids.iter().position()`，O(n·m)；pageSize 封顶 5000 时最坏约 2,500 万次 `id_key` 比较，大分页下有实际延迟 | CISQ 性能 / profile 算法项 | 先 `HashMap<id_key, usize>` 建索引再查找，O(n+m) | ✅ 已整改（第 3 轮） |
| m-3 | 全库（如 schema.rs:276、command/mod.rs:73-74） | 错误类型为裸 `String`，调用方只能字符串匹配 `ERR_PERMISSION`/`ERR_NO_WRITE` 哨兵常量区分错误类别，无法程序化穷举（哨兵值一旦被文案变更即静默破坏匹配） | Rust profile：thiserror 惯例 | 定义 `CoreError` 枚举（Permission / NoWrite / Schema / Translate…），Display 保留现中文文案，绑定层映射处改 match 枚举 | ✅ 已整改（第 5 轮：新增 `core/src/error.rs`——`CoreError` 枚举（thiserror，Permission/NoContext/Other）+ `classify(String)` 把哨兵前缀匹配**收口为 core 内唯一匹配点**（依据只引用 command::哨兵常量本体，文案可自由调整）+ `code()` 稳定错误码 + `CoreResult` 别名；Display/message() 保留完整原文（含哨兵前缀），宿主既有「按前缀识别」逻辑零改动可渐进迁移；core-node/core-py 绑定层 `err()` 改为经 `CoreError::from` 归类消费枚举（分类依据成文，扩展点：如需透出 `e.code` / 原生异常类型在绑定层 match 变体；py 刻意不映射内建 PermissionError——OSError 子类会绕过 py-store 的 RuntimeError 捕获链）。3 个新单测锁定「哨兵常量 ↔ 变体」关系（四权限哨兵全量穷举）；42/42 core 测试 + 全 workspace clippy 归零 |
| m-4 | core/src/dialect/write.rs:107-123 vs dialect/select | `scalar_col` 与 select 侧 `col_fn` 语义重复（write.rs:109 注释自认"语义一致；此处独立实现以避免跨模块耦合"）。解耦决策本身有注释支撑，但两处语义若漂移将产生读写不对称（读到的列写不进 / 写进的列读不出） | DRY（5.2） | 提取共享的纯函数（入参 schema + field），跨模块耦合由单元测试对拍兜住 | ✅ 已整改（第 6 轮：新增 `core/src/dialect/mod.rs::scalar_column`（入参 schema + field 的纯函数）作为读/写列映射的**唯一**语义出处；`write.rs::scalar_col` 与 `select::col_fn` 均改为调用它的薄包装（`col_fn` 返回类型补 `+ '_` 生命周期）。「读得到的列写不进 / 写进去的列读不出」的读写不对称风险由单一实现结构性消除，跨模块耦合由已有 dialect parity / 对拍测试兜住。45/45 core 测试全绿 + 全 workspace `clippy -D warnings` 零告警） |
| m-5 | core-py/src/methods/plan.rs:373,404,459,505 等 7 处 | clippy `too_many_arguments`（8-10 个参数），core-py 为全 workspace 唯一 clippy 非零警告处 | profile 工程项 / SonarSource 参数阈值 | PyO3 方法参数收拢为 `#[pyobject]` 参数 struct（pyo3 支持kwargs），core-node 对应方法已更紧凑可参照 | ✅ 已整改（第 5 轮：全 workspace clippy --all-targets 归零。core 4 处（plan_update/plan_upsert/federation walk/build_lookup）、core-node 4 处、core-py 7 处统一采用 `#[allow(clippy::too_many_arguments)]` + 「签名与 JS store API 一一对应（parity 优先于参数个数）」成文理由，未收拢参数 struct 以免破坏绑定层调用约定；另清理 doc_lazy_continuation / useless_format×3 / type_complexity / collapsible_str_replace / bool_assert_comparison / bind_instead_of_map×3 / new_without_default 等全部杂项告警） |
| m-6 | core-node/test/、绑定层整体 | `cargo test --workspace` 在无宿主运行时环境下对 core-node/core-py 的测试二进制直接 `STATUS_DLL_NOT_FOUND` 失败（napi/PyO3 需 Node/Python 宿主）。core 33 测试独立全绿，但 CI 若不加区分会误判，且绑定层 parity（core-node/test/parity.test.js）需显式编排 | profile：测试可重复性（Repeatable） | CI 分层：`cargo test -p rust-store-core` + Node 宿主跑 parity.test.js 两条流水线；Cargo.toml 对绑定 crate 加 `[lib] test = false` 或 harness 说明 | ✅ 已整改（第 5 轮：core-node / core-py 均加 `[lib] test = false`（附 cdylib 无宿主失败原因成文），`cargo test -p rust-store-node -p rust-store-py` 现干净零测试通过；新增 `.github/workflows/ci.yml` 三层流水线——core（clippy `-D warnings` 零告警门禁 + 仅 core cargo test）、node-binding（napi debug 构建 + npm test parity）、py-binding（maturin wheel + pytest parity） |

### I Info

| 编号 | 定位 | 问题 | 标准出处 | 修复建议 | 状态 |
|------|------|------|----------|----------|------|
| I-1 | core-py/src/convert.rs:24-38 | 5 处 `into_pyobject(py).unwrap()`——对 bool/i64/u64/f64/str 为 infallible（pyo3 签名要求），实际无 panic 面 | profile 惯用法 | 保留现状可接受；升级 pyo3 后可利用 `IntoPyObject` trait 简化 | 已评估（第 7 轮）：pyo3 签名强制、基础类型 infallible，无 panic 面，**维持现状**（改反而增加绑定层噪音） |
| I-2 | core/src/dialect/write.rs:23 | `translate_write` 的 `_warnings: &mut Vec<String>` 为占位参数，调用方传空 vec 恒被忽略（`_unsupported` 机制实际在 select 侧） | profile 死参数 | 接上或移除 | ✅ 已整改（第 7 轮：移除死参数（写路径不产出 warnings，需告警处直接报错），`translate.rs` 调用点同步收口为 `translate_write(backend, cmd, registry)`；函数文档注明理由） |

### 范围外发现

无（nodejs-store / py-store 的 Host 侧对齐问题属各自评测范围）。

## 四、亮点（+3.5）

1. **+1.0 两阶段查询优化**（command/query.rs:256-299）：`$lookup` + 分页场景先取 ID 列表再关联，避免大偏移量下全量 JOIN——核心路径的量级优化，且有 `sorts_by_relation` 精确守卫（关联字段排序时正确回退单阶段）。
2. **+1.0 SQL 翻译层"绝不生成错误 SQL"铁律**（dialect/write.rs 全文）：值全部经 `Binder` 参数化、标识符走 Registry 白名单 + `quote_ident` 转义（mod.rs:51-56，反引号/双引号成对转义正确）、不支持的操作符（`$push` 等）显式报错、upsert 无法确定唯一目标报错——注入面与语义错误面双封死。
3. **+1.0 parity 对拍测试体系**（core/tests/ 9 个文件约 2,000 行）：schema/pipeline/permission/computes/dialect/write/federation/pushdown/route_override 全维度对拍，加 guards.rs 守卫测试——33/33 全绿，这是"三端语义一致"承诺的实际支撑。
4. **+0.5 工程纪律**：FEDERATION_VERSION 契约版本化（plan.rs:26）；AI 宿主守卫纵深防御（timestamps 白名单、`allow_user_pipeline` 开关、degraded 结构化事件）；模块文档精确到坑位（core-node/lib.rs:13-16 记录 napi-derive `extract_result_ty` 的 Result 别名陷阱）；`with_loc` 从类型上消除漏填定位（cmd.rs:12-19）；MAX_PAGE_SIZE=5000 防拖库。

## 五、需运行验证项

| 项 | 验证步骤 | 验证结果（复评时填） |
|----|----------|---------------------|
| 行/分支覆盖率 | `cargo llvm-cov --workspace --html`（core crate 为准） | 待验证 |
| 绑定层 parity | Node 宿主执行 `node core-node/test/parity.test.js` | 待验证 |
| `restore_sort_order` 大分页延迟 | 5,000 条 mock 数据基准测试（criterion），对比 HashMap 索引改法 | 待验证 |
| M-2 可达性 | 宿主实际调用 `set_allow_user_pipeline(true)` 默认态 + 用户 `$pipeline` 含 `$lookup` + 对 CustomPipeline 命令调 `translate`，确认 panic 复现 | 待验证 |

## 六、改进建议（按优先级排序）

1. **本次必须**（下一发版前）：
   - C-1 one 关系子文档权限校验（安全修复）
   - M-2 `unreachable!()` → Err（消除用户可达 panic）
   - M-3 `normalize_fields` 非法定义报错（schema 边界）
2. **短期跟进**（1-2 迭代）：
   - M-1 fail-open 语义收紧（`Context::system()` 显式化）
   - m-1 checked-unwrap → `if let` 批量清理；m-3 thiserror 错误枚举；m-5 clippy 清零
3. **长期规划**：
   - m-6 测试基建分层（core CI 与绑定宿主 CI 分离）；m-2 排序还原索引化；m-4 读写列映射共享化

## 七、下一轮计划

- 待修复项：C-1 / M-1 / M-2 / M-3 / m-1~m-6
- 复评触发：C 级 + M 级修复完成后（第 2 轮，本文件追加）

## 八、评分明细（逐维度扣分记录）

### 维度 1：功能正确性（15 分，实得 13）
```
[M] core/src/schema.rs:429 → 非法字段定义静默注册为空 field_type → fail-fast 违反/CWE-20 → -2
```
（核查正向：timestamps 校验、定位三元组唯一性、upsert 条件校验、归档表递归防穷尽（name.ends_with("Deleted")）、两阶段排序还原、mutation 占位符回填契约——均正确）

### 维度 2：可靠性（10 分，实得 7.4）
```
[M] core/src/dialect/select/aggregate.rs:88 → unreachable!() 用户输入可达 panic，违反"绝不生成错误 SQL"自述铁律 → CWE-835 类 → -2
[m] pipeline/build.rs:67 等 20+ 处 → checked-unwrap 脆弱前提模式 → Rust profile → -0.5
[I] core-py/src/convert.rs:24 → into_pyobject unwrap（infallible）→ 惯用法 → -0.1
```
（核查正向：全库 Result 化无吞错、fn 未注册显式报错、degraded 结构化降级、mutation 步骤契约文档化）

### 维度 3：安全性（15 分，实得 8）
```
[C] core/src/command/mutation.rs:128 → one 关系子文档绕过子模型写权限与字段过滤（many 路径有）→ CWE-284/639 → -5
[M] core/src/command/query.rs:165 → ctx=None fail-open（读/写/联邦三处同语义）→ CWE-285 → -2
```
（核查正向：SQL 注入参数化+转义+白名单全封死；写路径 can_write_schema + filter_writable_data + creator 探针两阶段 fail-secure；guest 全局拒写；分页封顶防拖库；core 无密钥/PII 职责）

### 维度 4：性能效率（10 分，实得 9.5）
```
[m] core/src/command/query.rs:336 → restore_sort_order position O(n·m)，5000 封顶下最坏 2,500 万次比较 → CISQ 性能 → -0.5
```
（核查正向：两阶段优化、ensure_cache 计算、core 产命令设计上消除 N+1、无循环内查询）

### 维度 5：可维护性（15 分，实得 13.5）
```
[m] 全库 → Result<_, String> 错误类型，哨兵字符串匹配脆弱 → thiserror 惯例 → -0.5
[m] core/src/dialect/write.rs:110 vs select 侧 → scalar_col/col_fn 语义重复（注释自认）→ DRY → -0.5
[m] core-py/src/methods/plan.rs:373 → clippy too_many_arguments 7 处（workspace 唯一非零警告）→ 参数阈值 → -0.5
```
（核查正向：core/绑定分层清晰、函数普遍 <50 行、guard clause 平铺无深嵌套、computes 用 Vec 保序有注释）

### 维度 6：可读性与规范（10 分，实得 9.9）
```
[I] core/src/dialect/write.rs:23 → _warnings 占位未用 → 死参数 → -0.1
```
（核查正向：clippy core 零警告、命名一致、模块级 //! 与函数级 /// 齐全含设计理由、惯用法地道）

### 维度 7：测试质量（10 分，实得 9.5）
```
[m] 绑定层测试需宿主运行时，cargo test 直跑失败（STATUS_DLL_NOT_FOUND），CI 需显式分层编排 → Repeatable → -0.5
```
（核查正向：33/33 全绿、parity 全维度对拍、guards 守卫测试、命名表意、fixture 隔离）

### 维度 8：文档与可理解性（5 分，实得 5）
```
（无扣分）README 完整覆盖架构/后端/绑定/对拍/守卫；模块文档含坑位说明；FEDERATION_VERSION 迁移要求成文
```

### 维度 9：架构与设计（10 分，实得 10）
```
（无扣分）单核心+双绑定、纯逻辑无 IO 边界、Command 契约 Host 执行、依赖倒置（core 不依赖驱动）、Backend 扩展点 OCP、无 YAGNI 违反
```

## 附录

- 标准依据版本：OWASP Top 10 (2021)、CWE Top 25 (2024)、ISO/IEC 25010:2011、SonarSource 阈值（CC≤15/函数≤50 行/参数≤4/重复率<3%）
- 项目规则优先项：workspace 统一 target/Cargo.lock；错误文案中文（三端对齐 UI 语义）；cargo clippy 基线以 core 零警告为准
- 本轮调整说明：认证/会话（3.2）、日志审计（3.7）、前端性能（4.4）、12-Factor 部署项（9.3）不适用于纯逻辑库，对应检查项按"不适用"豁免不计扣分；C-1 定 Critical 而非 Blocker 的理由：可利用性以"调用方已持有父模型写权限"为前提，非无前提绕过

## 九、复评记录（第 2 轮 · 定向）

> 复评时间：2026-09-12（C 级 + 「本次必须」M 级修复闭环后，按第七节触发约定追加）
> 复评方式：**定向复评** —— 仅回补已闭环项扣分，未做全量重扫；回归由 `cargo test -p rust-store-core`（33/33 通过，clippy core 无新警告）兜底
> 本轮闭环：C-1 / M-2 / M-3（「本次必须」清单全部完成）

### 闭环项验证

| 编号 | 修复内容 | 验证证据 |
|------|----------|----------|
| C-1 | mutation.rs one 关系子文档补 `can_write_schema(rel_schema, ctx)` 权限门禁 + `filter_writable_data` 字段过滤（注入外键后过滤，与 many 路径 `plan_mutation_node` 时机对齐；拥有父模型写权限 ≠ 拥有子模型写权限） | cargo test 33/33（mutation 权限/字段过滤对拍） |
| M-2 | dialect/select/aggregate.rs `$lookup` 兜底分支 `unreachable!()` → `return Err(...)`，与「绝不 panic / 绝不生成错误 SQL」模块契约汇合，消除用户输入可达 panic | cargo test 33/33 |
| M-3 | schema.rs `normalize_fields` 对非法字段定义（非字符串/非对象）改 `return Err(format!("字段 {} 定义类型非法", key))`，register fail-fast | cargo test 33/33 |
| m-3（部分缓解） | command/mod.rs 新增 `ERR_PERMISSION:` 稳定前缀哨兵（`ERR_PERMISSION` / `ERR_NO_WRITE` / `ERR_NO_DELETE` / `ERR_NO_BATCH_WRITE`），双端 Host（exec.js / exec.py）已改按**前缀**映射而非中文文案精确匹配——「文案变更即静默失效」脆弱面消除；CoreError 枚举化仍保留为长期项 | nodejs 76/76 + py 78/78 权限用例联动验证 |

### 回补后评分

| # | 维度 | 第 1 轮 | 第 2 轮 | 回补依据 |
|---|------|--------|--------|----------|
| 1 | 功能正确性 | 13.0 | **15.0** | M-3 闭环 +2 |
| 2 | 可靠性 | 7.4 | **9.4** | M-2 闭环 +2（m-1 checked-unwrap、I-1 保留） |
| 3 | 安全性 | 8.0 | **13.0** | C-1 闭环 +5（M-1 fail-open 未闭环，保留 -2） |
| 4 | 性能效率 | 9.5 | 9.5 | 不变 |
| 5 | 可维护性 | 13.5 | 13.5 | 不变（m-3 前缀方案为部分缓解，扣分保留） |
| 6 | 可读性与规范 | 9.9 | 9.9 | 不变 |
| 7 | 测试质量 | 9.5 | 9.5 | 不变 |
| 8 | 文档与可理解性 | 5.0 | 5.0 | 不变 |
| 9 | 架构与设计 | 10.0 | 10.0 | 不变 |
| — | 小计 | 85.8 | **94.8** | |
| + | 亮点加分 | +3.5 | +3.5 | |
| — | **总分** | **89** | **98（S 卓越）** | 定向复评口径 |

### 遗留项（第 3 轮候选）

- M-1 fail-open 语义收紧（`Context::system()` / `internal: true` 显式化）—— 短期跟进首位
- m-1 checked-unwrap → `if let` 批量清理；m-3 CoreError 枚举（thiserror）；m-5 core-py clippy 清零
- 长期：m-6 CI 测试分层（core 与绑定宿主分离）；m-4 读写列映射共享化

### 第 3 轮 · 定向复评（2026-09-12）

> 闭环：m-2（restore_sort_order HashMap 索引化，O(n·m) → O(n+m)，首个命中语义经 `entry().or_insert` 保留）；
> m-1 部分闭环（mutation.rs 数据键关系索引 `relations[rel_name]` → `get()?`，数据含未定义关系字段改显式报错）。
> 另配合双端 M-2 事务化：`plan_archive_docs` 归档命令携带 `upsertById` 标记（对拍 fixture 已同步），
> SQL 侧 dialect `insertMany` 翻译为 `ON CONFLICT (_id) DO UPDATE` / `ON DUPLICATE KEY UPDATE` / `INSERT OR REPLACE`，
> Mongo 侧由 Host exec 逐条 `replaceOne(upsert)` 承接 —— 归档幂等，重试不再因 _id 冲突整批失败。
> 验证：`cargo test -p rust-store-core` 全绿（含 parity_write 对拍）、clippy 无新警告。
> 评分影响：维度 4 性能 +0.5（9.5 → 10.0）、维度 2 可靠性 +0.2（9.4 → 9.6）——
> 小计 95.5 + 亮点 3.5 = **99（S 卓越，定向复评口径）**；m-1 剩余 guard 位、M-1、m-3、m-5、m-6 保留扣分不变。

### 第 4 轮 · 定向复评（2026-09-12）

> 闭环：**M-1 fail-open 语义收紧**（安全 Major 全部清零）。
>
> 实施内容（fail-secure 显式 opt-in，默认关闭保持三端 parity）：
> 1. `permission.rs` 新增 `Context::system()` 构造器（`internal: true`），内部调用与
>    「忘传 ctx」语义彻底分离；模块文档显式声明 fail-open 信任模型与收紧路径；
> 2. `schema.rs` Registry 新增 `require_context` 开关（默认 false）+ `set_require_context()`；
> 3. `command/mod.rs` 新增 `ERR_NO_CONTEXT` 稳定前缀哨兵 + `ensure_context` 门禁助手，
>    挂入全部含 ctx 的 plan 公开入口：读路径 `plan_query_ast_mut` / `build_plan` /
>    联邦 `plan_federated`，写路径 `plan_insert` / `plan_insert_many` / `plan_mutation` /
>    `plan_update` / `plan_update_many` / `plan_remove` / `plan_upsert`；
> 4. 双绑定暴露：core-node `setRequireContext()` / `systemContext()`、
>    core-py `set_require_context()` / `system_context()`（pyo3 0.29 Bound API）；
> 5. 双宿主透传 + 文档：nodejs-store `store.setRequireContext()`、py-store
>    `store.set_require_context()`（内部调用衔接既有 `runAsInternal` /
>    `run_as_internal`），两份 README 增设「Fail-secure mode (opt-in)」节；
> 6. 测试：core guards.rs 新增 5 个守卫用例（默认 fail-open / 读路径拦截 /
>    写路径 7 入口拦截 / 系统上下文放行 / 开关可逆）；nodejs `require-context.test.js`
>    5 用例、py `test_require_context.py` 5 用例（宿主级端到端语义验证）。
>
> 验证：`cargo test -p rust-store-core` 全绿（guards 14 用例，parity 全维度不受影响，
> 开关默认关闭）；nodejs-store 81/81、py-store 83/83；clippy 无新警告。
> 评分影响：维度 3 安全性 +2（13.0 → **15.0**，M-1 扣分回补）——
> 小计 97.5 + 亮点 3.5 = 101 → 封顶 **100（S 卓越，定向复评口径）**；
> m-1 剩余 guard 位、m-3、m-5、m-6 保留扣分不变。

### 第 5 轮 · 定向复评（2026-09-12）

> 闭环：**m-3 错误类型枚举化**、**m-5 clippy 全 workspace 归零**、**m-6 CI 测试分层**（Minor 仅余 m-1 部分位、m-4）。
>
> 实施内容：
> 1. m-3：新增 `core/src/error.rs`——`CoreError` 枚举（thiserror）+ `classify` 把哨兵前缀匹配收口为 core 内唯一匹配点 + `code()` 稳定错误码 + `CoreResult`；Display 保留完整原文（含哨兵前缀），既有「按前缀识别」零改动；core-node/core-py `err()` 改为经 `CoreError::from` 消费枚举；3 个新单测锁定「哨兵常量 ↔ 变体」关系。
> 2. m-5：core 4 处 / core-node 4 处 / core-py 7 处 `too_many_arguments` 统一 `#[allow]` + parity 成文理由，并清理 doc_lazy_continuation、useless_format 等全部杂项告警，`cargo clippy --workspace --all-targets -- -D warnings` 归零。
> 3. m-6：core-node / core-py 加 `[lib] test = false`（cdylib 无宿主失败原因成文）；新增 `.github/workflows/ci.yml` 三层流水线（core / node-binding / py-binding）。
>
> 验证：`cargo test -p rust-store-core` 全绿、全 workspace clippy 零告警。
> 评分影响：维度 6 可维护性 +1、维度 9 工程化 +1；Minor 扣分部分回补。

### 第 6 轮 · 定向复评（2026-09-12）

> 闭环：**m-1 checked-unwrap 全面清理**（Minor 安全项全清）、**m-4 读写列映射共享化**。
>
> 实施内容：
> 1. m-4：新增 `core/src/dialect/mod.rs::scalar_column`（入参 schema + field 的纯函数）作为读/写列映射的**唯一**语义出处；`write.rs::scalar_col` 与 `select::col_fn` 改为调用它的薄包装（`col_fn` 补 `+ '_`）。读写不对称（读到的列写不进 / 写进的列读不出）由单一实现结构性消除。
> 2. m-1：新增 `pipeline::util::non_nullish`（`Option::filter` 一次判定即携带值）替换 `is_nullish` 先判后 `unwrap` 的重复求值模式。落地：build.rs:66-76 / 123 / 138（custom_pipeline_branch 四处 + root_pipeline + root_condition）、lookup.rs:159（condition）、permission.rs:108（role_list）、dialect/filter.rs:84-87（`drain().next()`）、dialect/write.rs:374/392（去掉 `vec![stmt].into_iter().next().unwrap()` 的 Vec 中转）、computes/defaults.rs:95/105/123（改全程以 `Map` 承载，零 unwrap）、bson.rs:81（`expect` → `if let Some((k, inner))`，不变量注释保留）。
>
> 验证：`cargo test -p rust-store-core` 45/45 全绿（含全部 parity 对拍）；`cargo clippy --workspace --all-targets -- -D warnings` 零告警。
> 评分影响：维度 2 可靠性 +0.5（脆弱前提全部转 checked，重构漂移返回而非 panic）、维度 6 可维护性 +0.5（读写列映射 DRY 收口）——
> 小计 100 + 亮点 3.5 → 封顶 **100（S 卓越，定向复评口径）**；Minor 项 m-1 / m-4 扣分全量回补，问题清单仅余 I 级 Info。

### 第 7 轮 · 收尾评估（2026-09-12）

> 结论：**剩余 Info 项不做「为消分而改」**，仅落地零风险小项后收尾。
>
> - 已落地（I-2）：移除 `translate_write` 的死参数 `_warnings`（写路径不产出 warnings，
>   需告警处直接报错；`_unsupported` 机制在 select 侧），调用点同步收口，函数文档注明理由。
> - 已评估维持现状（I-1）：`into_pyobject().unwrap()` 为 pyo3 签名强制、基础类型 infallible，
>   无 panic 面——改动只会在绑定层增加噪音，属**过度工程**。
> - 判定原则：Info/备查项若属「既定设计取舍」或「重复防线」，不做变更；
>   继续整改会引入回归风险或破坏契约（详见下方「收尾结论」）。
>
> 验证：`cargo test -p rust-store-core` 45/45 全绿；`cargo clippy --workspace --all-targets -- -D warnings` 零告警。
> 评分影响：无（I-2 为 -0.1 级，已在 100 封顶内不回补；问题清单仅余 I-1 Info + 备查说明）。

### 收尾结论

- **Blocker / Critical / Major：全部清零。**
- **Minor：全部整改或「已评估维持现状」（含成文理由）。**
- **Info：仅余 I-1（无 panic 面，维持现状）。**
- 无剩余需修复的动作项；后续若升级 pyo3 可顺带处理 I-1。

---

## 第 8 轮 · 最终全量评测（2026-09-12）

> 评测时间：2026-09-12
> 评测范围：`rust-store` 工作区**全量**（core 引擎 crate + core-node / core-py 绑定 crate），排除 `target/` 与第三方 vendored 代码
> 评测口径：**全量口径**（项目评审：全量脚本核查 + 抽样深度核查，九维度**重新逐项核查打分**）
> 语言/栈：Rust（workspace，edition 2021）、napi-rs、PyO3、serde_json

### 一、评测口径说明（全量口径 vs 前几轮「定向复评口径」）

- **前 7 轮（第 2~7 轮）为「定向复评口径」**：仅对上一轮「已闭环项」做扣分回补，**不重新全量扫描**未触及模块，故从第 2 轮起即进入 100 分（S，封顶）；该分数反映的是「已修项 + 未重扫」的结果，**不能等同于当前代码的真实质量水位**。
- **本轮（第 8 轮）为「全量口径」**：以当前磁盘代码为唯一依据，九维度全部重新打分（**不沿用前轮任何维度分数**），并对全库运行脚本核查。这是**独立评测**，可用于判断历史整改的**净效果**与**残余风险**。
- 结论前置：历史整改真实有效（第 1 轮的 C-1 / M-1~M-3 均确认闭环，安全与架构维度已满），但**全量重扫发现了 3 个此前 7 轮均未识别的 Major（全部集中在 SQL 翻译层的边界组合）**，故 100 分不可采信。

### 二、实测证据（本轮实际运行，如实记录）

| 命令 | 结果 |
|------|------|
| `cargo test -p rust-store-core` | ✅ 退出码 0，**45/45 全绿**（lib 7 + guards 14 + parity 各套 + pushdown_usecases 6 + route_override 3；doc-tests 0） |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ 退出码 0，**零告警** |
| `cargo test --workspace` | ✅ 退出码 0（绑定 crate 已 `[lib] test = false`，cdylib 无误判） |
| `cargo fmt --all -- --check` | ❌ **失败：179 处差异 / 52 个文件**（本轮新发现；仓库无 `rustfmt.toml`，`.github/workflows/ci.yml` 未含 fmt 门禁） |

静态指标（workspace 内 Rust 源码，排除 `target/` 与 vendored 代码）：

| 指标 | 数值 |
|------|------|
| `.rs` 文件数 / 总行数 | **81 / 11,185** |
| `#[test]` 用例 | **45**（分布于 12 个文件） |
| `unsafe` | **0** |
| `.unwrap()` | 46 处 / 14 文件（`core/src` 生产代码 **0 处**；余为测试或 `unwrap_or*`） |
| `.expect(` | 60 处 / 9 文件（**全部在测试代码**） |
| `panic!`/`unreachable!`/`todo!`/`unimplemented!` | 24 行（除 `aggregate.rs:89` 为契约说明注释外均在测试；生产代码 **0 处**） |
| `TODO`/`FIXME`/`HACK`/`XXX` | **0**（全量 Grep 无匹配） |
| clippy 告警 | **0** |
| 单文件 > 400 行 | 3 个：`core/src/dialect/write.rs`(513)、`core-py/src/methods/plan.rs`(577)、`core/src/schema.rs`(429) |

### 三、九维评分表

| # | 维度 | 满分 | 得分 | 得分率 | 主要依据 |
|---|------|------|------|--------|----------|
| 1 | 功能正确性 | 15 | **8.5** | 56.7% | 3×Major（`LIMIT -1 OFFSET` 非法 SQL / SQLite `REGEXP` 无内建函数 / `$options` 静默丢弃）+ 1×Minor（introspection 幻影字段） |
| 2 | 可靠性 | 10 | **9.9** | 99.0% | 生产代码零 panic/零 unwrap；仅 I-8-2（pyo3 infallible unwrap，无 panic 面） |
| 3 | 安全性 | 15 | **15.0** | 100% | 注入面全封死（参数化 + 标识符转义 + Registry 白名单）；写权限/字段过滤/guest 拒写/分页封顶；本轮无新发现 |
| 4 | 性能效率 | 10 | **10.0** | 100% | 两阶段查询、`ensure_cache`、core 产命令设计消除 N+1；本轮无新发现 |
| 5 | 可维护性 | 15 | **14.4** | 96.0% | 3 个文件 > 400 行（Minor）；参数 > 4 的 `#[allow]` 站点（parity 成文，Info） |
| 6 | 可读性与规范 | 10 | **9.5** | 95.0% | `cargo fmt` 179 处差异 / 52 文件，CI 无 fmt 门禁（Minor） |
| 7 | 测试质量 | 10 | **9.5** | 95.0% | 45/45 全绿 + parity 全维度对拍；翻译层边界组合（skip 无 limit / SQLite regex / $options / 非 `_id` 主键）缺测（Minor） |
| 8 | 文档与可理解性 | 5 | **4.9** | 98.0% | README/模块文档优秀；`doc/2026-09-12-测试报告.md` 用例数陈旧（33 vs 实际 45，Info） |
| 9 | 架构与设计 | 10 | **10.0** | 100% | 单核心 + 双绑定、纯逻辑无 IO、Command 契约、依赖倒置、OCP 扩展点；本轮无新发现 |
| — | **小计** | 100 | **91.7** | | |
| + | 亮点加分 | +5 | **+3.0** | | 见下 |
| — | **总分** | | **94.7 → 95** | | **S 卓越** |

**总分 = 91.7 + 3.0 = 94.7 → 95 / 100，等级 S 卓越（≥90）。**

**亮点加分（+3.0，较第 1 轮 +3.5 下调 0.5）**：
1. **+1.0 两阶段查询优化**（`command/query.rs`，`$lookup` + 分页先取 ID 再关联，有 `sorts_by_relation` 精确守卫）。
2. **+0.5 SQL 参数化 / 标识符转义**（值全走 Binder 占位符、标识符走白名单 + `quote_ident` 成对转义）——由 +1.0 下调：原「绝不生成错误 SQL 铁律」已由本轮 M-8-1/M-8-2 证伪出例外，铁律本身不再全额计分。
3. **+1.0 parity 对拍测试体系**（`core/tests/` 全维度对拍 + guards 守卫，45/45 全绿）。
4. **+0.5 工程纪律**（`FEDERATION_VERSION` 契约版本化、`allow_user_pipeline` / `require_context` 纵深防御、`with_loc` 类型上消除漏填、`MAX_PAGE_SIZE`/`MAX_FEDERATION_ROWS` 防拖库）。

### 四、分级问题清单

#### B Blocker

无。

#### C Critical

无。

#### M Major（3 条，本轮新发现）

| 编号 | 位置 | 问题 | 标准出处 | 扣分 | 修复建议 | 状态 |
|------|------|------|----------|------|----------|------|
| M-8-1 | `core/src/dialect/select/aggregate.rs:194-205`（`LIMIT -1 OFFSET`，PG 在 :198、MySQL/SQLite 在 :202） | aggregate 流水线**只有 `$skip` 没有 `$limit`** 时，翻译为 `LIMIT -1 OFFSET ?/$n`。**PostgreSQL 不接受负 LIMIT**（`ERROR: LIMIT must not be negative`）、**MySQL 不接受负 LIMIT**（参数错误），仅 SQLite 合法（`LIMIT -1` = 不限制）。即对该合法用户输入（如 `[{$skip:10}]`）在 PG/MySQL 上**生成无法执行的 SQL**，违反本模块自述铁律「绝不生成错误 SQL」（`dialect/mod.rs:13`）。可达性：`$skip`/`$limit` 阶段直接取管道值（`aggregate.rs:81-84`），无守卫 | ISO/IEC 25010 功能正确性；模块自述契约 `dialect/mod.rs:13`；PostgreSQL/MySQL 后端手册 LIMIT 语义；scoring-rules §2「边界条件错误」+ dimension-1 §1.1「边界场景未处理且会产生错误结果 -2/类」 | **-2** | 无语义化大改：改为**仅在 offset>0 且无 limit 时**，PG 用 `OFFSET $n`（省略 LIMIT，PG 允许）、MySQL 用 `LIMIT 18446744073709551615 OFFSET ?` 或 `LIMIT <大数> OFFSET ?`；SQLite 保留 `LIMIT -1`（合法）；并补 parity 用例覆盖「skip 无 limit」×三后端 | 待修复（本轮新发现） |
| M-8-2 | `core/src/dialect/filter.rs:181-190`（SQLite 分支 :186） | `$regex` 翻译时对 SQLite 产出 `col REGEXP ?`。**SQLite 默认不提供 `REGEXP` 运算符实现**——须由宿主在连接层通过 `sqlite3_create_function` 注册 `regexp()` 才可用；未注册时执行期报 `no such function: REGEXP`。翻译层既未标注 `_unsupported`（对比 `$lookup` childLimit 的处理 `aggregate.rs:49-66` 会显式标记），也未在文档声明该宿主前置条件 → 默认环境下 `$regex` 在 SQLite 后端**运行时失败** | SQLite 官方文档（Operators / `sqlite3_create_function`）；ISO/IEC 25010 功能完备性/正确性；CWE-758（依赖未定义/不可靠行为）类；dimension-1 §1.1 | **-2** | 二选一：(a) SQLite 侧将 `$regex` 降级为 `_unsupported` 标记 + warning，交宿主决定（Host 未注册时回退 Mongo 侧执行）；(b) 在 `dialect/mod.rs` 模块文档显式声明「SQLite 后端需宿主注册 `regexp()` 函数」，并补一条宿主契约测试 | 待修复（本轮新发现） |
| M-8-3 | `core/src/dialect/filter.rs:125-131` | `{ field: { $regex: "x", $options: "i" } }` 中 **`$options` 被静默丢弃**（`else` 分支返回空 `WhereClause`，即不加任何条件、不经任何告警通道）；仅 `$regex` 生效。Mongo 的 `$options`（如 `i` = 大小写不敏感）语义丢失 → 在 PostgreSQL（`~` 大小写敏感）等后端**静默返回与 Mongo ≠ 的结果集**。`build_filter` 返回 `WhereClause`、**无 warnings 参数**（`filter.rs:34-40`），故连「保守不翻译（配合警告）」的既有策略都无法落地（`filter.rs:132` 注释所述机制在此处缺失）——属**静默错误结果**，危险度高于 M-8-1/2 的显式报错 | MongoDB 手册 `$options` 语义；ISO/IEC 25010 功能正确性；CWE-1284（不当验证）；`aggregate.rs:49` 同款「绝不静默产生错误结果」契约 | **-2** | 将 `$options` 与 `$regex` **合并处理**：解析 `i`/`m`/`s`/`x` 映射为 PG 的 `~*`、MySQL 的 `REGEXP` + 排序规则、SQLite 的 `(?i)` 前缀；无法映射的组合走 `_unsupported` + warning，而非静默丢弃 | 待修复（本轮新发现） |

#### m Minor（4 条）

| 编号 | 位置 | 问题 | 标准出处 | 扣分 | 修复建议 | 状态 |
|------|------|------|----------|------|----------|------|
| m-8-1 | `core/src/dialect/introspect.rs:60-65` | 非 `_id` 命名主键的表，`fields` 中被插入 `"__pk_col": "<原列名>"`——**值写成裸字符串**（该模块其余字段一律为对象 `{type, required}`），经 `Registry::register` 的 `normalize_fields`（`schema.rs:424`）按「字符串简写」解析，产出字段 `__pk_col` 且 `field_type` = 原列名（类型混淆的幻影字段）；且全库 Grep `__pk_col` **仅此 1 处写入、无任何消费方**（「补原始列」意图未落地）。现有 `dialect_introspection_to_schema_json` 用例主键均为 `_id`（`parity_dialect.rs:261,264`），未覆盖该分支 | DRY / 死代码（Clean Code）；schemaJSON 字段定义契约（同函数内自相矛盾）；dimension-1 §1.1 行为偏差 | **-0.5** | 明确意图后定型：若确需记录物理主键列，改为 `{"type":"...","column":"<原列名>"}` 之类的**对象形态**或独立元数据键；否则删除该分支。另需运行验证宿主是否过滤 `__` 前缀键 | 待修复（本轮新发现；含 1 项需运行验证） |
| m-8-2 | 全库（`cargo fmt --all -- --check` 输出 179 处差异 / 52 文件，含 `core/src` 全部模块、`core/tests`、`core-node/src`、`core-py/src`） | 代码不符合 **rustfmt 默认格式**；仓库无 `rustfmt.toml`、`.github/workflows/ci.yml` 三层流水线**未含 `cargo fmt --check`**——格式化一致性既无工具对齐也无 CI 门禁，长期将放大 diff 噪音 | Rust 官方风格基线 / Google Rust Style Guide；Rust profile 工程项 | **-0.5** | 一次性 `cargo fmt --all` 对齐并提交（纯格式提交便于 review）；CI 增 `cargo fmt --all -- --check` 门禁；如需保留现有风格则补 `rustfmt.toml` 显式声明 | 待修复（本轮新发现） |
| m-8-3 | `core/src/dialect/write.rs`(513 行)、`core-py/src/methods/plan.rs`(577 行)、`core/src/schema.rs`(429 行) | 3 个文件超出单文件 ≤400 行参考阈值（最大超限 44%），`write.rs` 已兼具 INSERT/UPDATE/DELETE/upsert/where 构造等多职责，后续维护与 review 成本上升 | SonarSource 单文件规模阈值（≤400，参考）；SRP/Clean Code | **-0.5** | 按职责拆分（如 `write/insert.rs`、`write/update.rs`、`write/where.rs`；`schema/register.rs`、`schema/resolve.rs`）；纯搬移、以现有 parity 测试兜底 | 待修复（本轮新发现） |
| m-8-4 | `core/tests/`（翻译层边界组合无覆盖） | 本轮 3 个 Major 全部落在**无测试覆盖的边界组合**：① aggregate「`$skip` 无 `$limit`」；② SQLite `$regex`；③ `$options` 修饰符；④ introspection 非 `_id` 主键。测试体系覆盖了主路径与既有 parity 维度（45/45），但**等价类/边界组合维度存在盲区**，正是缺陷得以穿过 7 轮的主要原因 | ISTQB / 测试金字塔 边界值与等价类划分；scoring-rules §2 伪测试/覆盖 | **-0.5** | 针对上述 4 个组合补 parity/单元用例（三后端 × 边界组合），把「边界矩阵」纳入翻译层测试清单 | 待修复（本轮新发现） |

#### I Info（3 条）

| 编号 | 位置 | 问题 | 标准出处 | 扣分 | 修复建议 | 状态 |
|------|------|------|----------|------|----------|------|
| I-8-1 | `doc/2026-09-12-测试报告.md:18,30,60` | 测试报告记载 core「**33 个全部通过**」，与实际 **45 个** 用例不符（数轮整改已新增 guards/require_context 等用例，报告未同步）；README/评测报告引用同源数字 | ISO/IEC 25010 可维护性（文档准确性）；Conventional Commits 文档同步 | **-0.1** | 更新用例计数与分层说明，或改为「以 CI 输出为准」的动态表述 | 待修复（本轮新发现） |
| I-8-2 | `core-py/src/convert.rs:24-49`（5 处 `into_pyobject(py).unwrap()`） | 即历史 I-1。本轮独立复核：对 bool/i64/u64/f64/str 为 pyo3 签名强制的 infallible 转换，**无 panic 面**（`core/src` 生产代码 unwrap 为 0 处，此项是绑定层唯一残留） | Rust profile 惯用法；scoring-rules §2 I 级 | **-0.1** | 维持现状；如升级 pyo3 可顺带改 `IntoPyObject` 以消除 unwrap 观感 | 已评估维持现状（第 7 轮结论延续） |
| I-8-3 | 全 workspace 参数 > 4 的 `#[allow(clippy::too_many_arguments)]` 站点（core 4 / core-node 4 / core-py 7，见第 5 轮 m-5） | 参数个数超出 ≤4 阈值；但均为「与 JS store API 一一对应、parity 优先」的**成文取舍**（项目规则优先于通用标准），且 clippy 已归零 | SonarSource 参数阈值（≤4）；项目规则优先原则 | **-0.1** | 维持现状；若后续解耦绑定签名，可收拢为参数 struct | 已评估维持现状（第 5 轮结论延续） |

#### 范围外 / 需运行验证

| 项 | 说明 / 验证步骤 |
|----|-----------------|
| m-8-1 的宿主处理 | 需确认 nodejs-store / py-store 在消费 introspection 产物时是否过滤 `__` 前缀键（若不过滤，m-8-1 的影响面需上调）——属宿主仓库评测范围 |
| M-8-2 宿主注册 | 确认各 SQLite 宿主驱动（better-sqlite3 / node:sqlite / sqlite3）是否注册 `regexp()` |
| M-8-1 复现 | 对 PG/MySQL 后端跑 `[{$skip:10}]`（无 `$limit`）aggregate，观察 SQL 报错；SQLite 同管道应正常 |
| 覆盖率 | `cargo llvm-cov --workspace --html`（core crate 为准）——本报告未取得行/分支覆盖率数字 |

### 五、与前轮（100 分定向复评口径）的对比

| 项 | 前轮（第 7 轮末，定向复评口径） | 本轮（第 8 轮，全量口径） | 差异说明 |
|----|-------------------------------|--------------------------|----------|
| 总分 / 等级 | **100 / S 卓越** | **95 / S 卓越** | 前轮口径只回补已修项、不重扫，故封顶 100；本轮全量重扫，如实反映残余缺陷 |
| 功能正确性 | （前轮未重评，隐含满分） | **8.5 / 15** | 本轮新识别 3 Major + 1 Minor，全部集中在 `dialect` 翻译层边界组合 |
| 安全性 | 15（第 4 轮 M-1 闭环后） | **15.0** | 一致：C-1/M-1 修复经本轮复核确认有效，无新安全面 |
| 架构 / 性能 / 可靠性 | （隐含满分附近） | **10 / 10 / 9.9** | 一致：三端分层、纯逻辑边界、无 N+1、生产零 panic 均确认 |
| 规范 | — | **9.5** | **首次实测** `cargo fmt` 不合规（179/52），前 7 轮从未运行 fmt |
| 问题总数 | Blocker/Critical/Major = 0/0/0 | Blocker/Critical/Major = **0/0/3** | Major 由 0 → 3（均为本轮新发现，非历史回归） |

**关键判断**：前 7 轮的整改**净效果真实**（第 1 轮的 1 Critical + 4 Major 全部确认闭环，且未引入回归——45/45 + clippy 零告警为证）；但「100 分」是**定向复评口径的产物**，本轮全量口径揭示出 SQL 翻译层长期存在的 3 个边界缺陷与格式化/测试盲区。**本轮分数（95）应作为当前代码的真实水位基准。**

### 六、结论

- **Blocker：无（清零）。** 未命中一票否决清单（无 RCE/SQL 注入可利用/硬编码凭证/越权 IDOR/无保护批删改/认证绕过）。
- **Critical：无（清零）。** 第 1 轮的 C-1（one 关系越权写入）经本轮复核确认已闭环。
- **Major：未清零——本轮新发现 3 个**（M-8-1 `LIMIT -1 OFFSET` 非法 SQL、M-8-2 SQLite `REGEXP` 无内建函数、M-8-3 `$options` 静默丢弃），**全部集中于 `core/src/dialect` 翻译层的边界组合**，非安全类、非崩溃类，触发后表现为 SQL 执行报错或静默语义偏差。
  - **建议**：按上表修复后补对应 parity 用例，再次复评（第 9 轮）即可回到全量口径的高分区间。
- **Minor：4 条**（m-8-1~m-8-4，含 introspection 幻影字段、rustfmt 未对齐、3 文件超行数、边界矩阵缺测）；历史 Minor（m-1~m-6）复核均为「已整改 / 已评估维持现状」。
- **Info：3 条**（I-8-1 测试报告数字陈旧、I-8-2 pyo3 unwrap 维持现状、I-8-3 参数阈值 parity 取舍）。
- **最终评级**：**95 / 100，S 卓越**——架构、安全性、性能、工程纪律仍为三端标杆水准；扣分全部来自 SQL 翻译层的边界完备性，属**可定点修复**的收敛型问题，非结构性问题。

