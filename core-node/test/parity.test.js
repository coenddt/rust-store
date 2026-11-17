'use strict';

/**
 * core-node 绑定 parity 对拍
 *
 * 回放 5 套 fixtures（pipeline / commands / computes / fnfns / federation）
 * 与其黄金基准，逐条深比较 Rust 绑定层的输出——语义与 core/tests/parity*.rs 完全一致：
 *   - pipeline   : `buildPipeline` 的 tokens / ast / pipeline / projection
 *   - commands   : `planQuery` / `planQueryWithCount` / `resolvePage` /
 *                  `restoreSortOrder` / `planInsert` / `planExists` / `planCount`
 *   - computes   : `processNode` / `injectDepends` + `stripDepInjected` / `permission.*`
 *   - fnfns      : 同步 fn 走 `setFn` 回调桥；asyncFn 由 Host 执行
 *                  （`asyncFnRefs` 取标识，`prepareQuery` + `stripQuery` 两段式）
 *
 * 运行：node --test core-node/test/parity.test.js
 */

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { isDeepStrictEqual } = require('node:util');

const { Registry } = require('./_binding');
const { fns, asyncFns } = require('../../tools/test-fns.js');

const FIXTURES = path.join(__dirname, '..', '..', 'fixtures');

const load = (p) => JSON.parse(fs.readFileSync(p, 'utf8'));
const has = (o, k) => Object.prototype.hasOwnProperty.call(o, k);

/** 建绑定实例并注册 schema / 全部同步 fn 回调（异步回调由 Host 侧直接调用） */
function makeRegistry(fx) {
  const reg = new Registry();
  const schemas = fx.schemas || [];
  if (schemas.length) {
    // 定义文件零落点：用例内 schema 的 `datasource` 由本夹具读取，作为 Location 注入
    reg.registerBatch(
      schemas.map((s) => ({
        defn: s,
        location: { source: s.datasource || 'default', database: null, schema: null },
      })),
    );
  }
  // 档位：缺省 standard；用例可声明 `"profile": "text2query"` 覆盖（双门禁分离）
  if (fx.profile) reg.setProfile(fx.profile);
  for (const [ref, fn] of Object.entries(fns)) reg.setFn(ref, fn);
  return reg;
}

/** 依次执行 Host 侧 asyncFn 回调（对齐 core 的 `run_async_fns`） */
function runAsyncFns(refs, items, ctx) {
  for (const ref of refs) {
    const fn = asyncFns[ref];
    if (!fn) throw new Error(`未注册的异步测试函数: ${ref}`);
    fn(items, ctx);
  }
}

function fmt(v) {
  return JSON.stringify(v);
}

function expectDeepEqual(failures, label, actual, want) {
  if (!isDeepStrictEqual(actual, want)) {
    failures.push(`  ${label}\n    want: ${fmt(want)}\n    got : ${fmt(actual)}`);
  }
}

// ─── pipeline ───────────────────────────────────────────────

/** 单条 pipeline 用例执行 → { out } 或 { err }（绑定异常不吞，交调用方分流） */
function runPipelineCase(fx) {
  try {
    return { out: makeRegistry(fx).buildPipeline(fx.gql || '', fx.params || {}, fx.context ?? null) };
  } catch (e) {
    return { err: e };
  }
}

/** pipeline 四键深比较（黄金基准缺键视为 null；差异按 key 汇总为一行） */
function comparePipelineKeys(fx, g, out, failures) {
  const lines = [];
  for (const k of ['tokens', 'ast', 'pipeline', 'projection']) {
    expectDeepEqual(lines, k, out[k], has(g, k) ? g[k] : null);
  }
  if (lines.length) failures.push(`[${fx.name}] 结果不一致\n${lines.join('\n')}`);
}

test('core-node parity: pipeline', () => {
  runParitySuite('pipeline', path.join('expected', 'cases.json'), (g, fx, failures) => {
    const { out, err } = runPipelineCase(fx);
    if (fx.expect_error) {
      if (!err) failures.push(`[${fx.name}] 期望报错但成功返回`);
      return;
    }
    if (err) {
      failures.push(`[${fx.name}] 绑定报错: ${err.message}`);
      return;
    }
    comparePipelineKeys(fx, g, out, failures);
  });
});

// ─── commands ───────────────────────────────────────────────

/** 把命令里 `{{step.<N>._id}}` 占位符替换为第 N 步模拟执行的 `_id` */
function substituteStepPlaceholders(value, resolved) {
  if (typeof value === 'string') {
    const m = value.match(/^\{\{step\.(\d+)\._id\}\}$/);
    if (m && resolved[Number(m[1])] !== undefined) return resolved[Number(m[1])];
    return value;
  }
  if (Array.isArray(value)) return value.map((v) => substituteStepPlaceholders(v, resolved));
  if (value && typeof value === 'object') {
    const out = {};
    for (const [k, v] of Object.entries(value)) out[k] = substituteStepPlaceholders(v, resolved);
    return out;
  }
  return value;
}

/** 写路径公共环境：ctx / now / 回喂后的 updatedDoc */
function writeEnv(fx) {
  return { ctx: fx.context ?? null, now: fx.now ?? 0, updated: fx.updatedDoc ?? null };
}

/** 宿主回喂结果（无 updatedDoc ⇒ null） */
function returnsOf(reg, fx, env) {
  return env.updated == null ? null : reg.applyWriteDefaults(fx.model, env.updated);
}

/** update：creator 受限写入 ⇒ core 先返回探针（宿主回喂后二次规划），黄金基准即 needsProbe 信封 */
function replayUpdate(reg, fx, env) {
  const out = reg.planUpdate(
    fx.model || '', fx.condition ?? null, fx.data ?? null, fx.options ?? null, env.now, env.ctx,
  );
  if (out.needsProbe) return out;
  return { commands: [out.command], returns: returnsOf(reg, fx, env) };
}

function replayUpdateMany(reg, fx, env) {
  const out = reg.planUpdateMany(fx.model || '', fx.condition ?? null, fx.data ?? null, env.now, env.ctx);
  return { command: out.command, returns: { modifiedCount: fx.modifiedCount ?? 0 } };
}

/** remove 的归档命令段：findCommand 存在才归档，docs 非空才追加 archive 命令并计 archivedCount */
function archiveCommands(reg, fx, plan, env) {
  const commands = [];
  let archivedCount = 0;
  if (plan.findCommand != null) {
    commands.push(plan.findCommand);
    const docs = fx.docs || [];
    if (docs.length) {
      commands.push(reg.planArchiveDocs(fx.model || '', docs, env.now).command);
      archivedCount = docs.length;
    }
  }
  return { commands, archivedCount };
}

/** remove：creator 受限删除 ⇒ 先返回探针（同上） */
function replayRemove(reg, fx, env) {
  const plan = reg.planRemove(fx.model || '', fx.condition ?? null, env.ctx);
  if (plan.needsProbe) return plan;
  const { commands, archivedCount } = archiveCommands(reg, fx, plan, env);
  commands.push(plan.deleteCommand);
  return { commands, returns: { deletedCount: fx.deletedCount ?? 0, archivedCount } };
}

function replayUpsert(reg, fx, env) {
  const out = reg.planUpsert(
    fx.model || '',
    fx.condition ?? null,
    fx.data ?? null,
    fx.options ?? null,
    env.now,
    (fx.newIds || [''])[0] || '',
    env.ctx,
  );
  return { command: out.command, returns: returnsOf(reg, fx, env) };
}

/** 单步 mutation 命令的模拟结果（insertOne → doc；findOneAndUpdate → 回喂文档） */
function mutationStepResult(command, env) {
  if (command.kind === 'insertOne') return command.doc ?? null;
  if (command.kind === 'findOneAndUpdate') return env.updated;
  throw new Error(`未支持的 mutation 步骤命令: ${command.kind}`);
}

function replayMutation(reg, fx, env) {
  const plan = reg.planMutation(fx.model || '', fx.data ?? null, env.now, fx.newIds || [], env.ctx);
  const resolved = [];
  const commands = [];
  let rootResult;
  for (const step of plan.steps) {
    const command = substituteStepPlaceholders(step.command, resolved);
    const result = mutationStepResult(command, env);
    if (rootResult === undefined) rootResult = result;
    resolved.push(result ? result._id ?? null : null);
    commands.push(command);
  }
  const returns = rootResult == null ? null : reg.applyWriteDefaults(fx.model, rootResult);
  return { commands, returns };
}

/** 写路径分派表：kind → 比较结构（未知 kind 返回 null，表示非 write 用例） */
const WRITE_REPLAYERS = {
  insert_many: (reg, fx, env) => reg.planInsertMany(fx.model || '', fx.docs || [], env.now, fx.newIds || [], env.ctx),
  update: replayUpdate,
  update_many: replayUpdateMany,
  remove: replayRemove,
  upsert: replayUpsert,
  mutation: replayMutation,
};

/** 写路径统一回放：Host 模拟执行命令并回喂结果（对齐 parity_write.rs） */
function replayWriteCase(reg, fx) {
  const replayer = WRITE_REPLAYERS[fx.kind];
  return replayer ? replayer(reg, fx, writeEnv(fx)) : null;
}

/** commands / federation 共用的黄金基准期望口径：error 信封 or result or null */
function goldenWant(g) {
  return g.error === true ? { error: true } : has(g, 'result') ? g.result : null;
}

/** planInsert 的比较结构（commands / fnfns 两分派器同构段）：返回 {command, returns, newId} */
function planInsertShape(reg, fx, ctx) {
  const out = reg.planInsert(
    fx.model || '',
    fx.data ?? null,
    fx.now ?? 0,
    fx.newId || '',
    ctx,
  );
  const command = out.command ?? null;
  return {
    command,
    returns: out.returns ?? null,
    newId: command && command.doc ? command.doc._id ?? null : null,
  };
}

/**
 * parity 套件统一骨架（pipeline/commands/computes/fnfns/federation/local 六套件共用）：
 * 加载 cases/expected → 数量与顺序断言 → 逐条 execCase 收集 failures → 汇总断言。
 * 各套件错误消息文案（`${suite} 对拍失败 …`）经 suite 参数原样保留；
 * goldensRel 兼容 pipeline 的特殊路径（expected/cases.json，其余为 <suite>/expected.json）。
 */
function runParitySuite(suite, goldensRel, execCase) {
  const cases = load(path.join(FIXTURES, suite, 'cases.json'));
  const goldens = load(path.join(FIXTURES, goldensRel));
  assert.equal(cases.length, goldens.length, '输入与黄金基准用例数不一致');

  const failures = [];

  cases.forEach((fx, i) => {
    const g = goldens[i];
    assert.equal(fx.name, g.name, '用例顺序不一致');
    execCase(g, fx, failures);
  });

  assert.equal(failures.length, 0, `${suite} 对拍失败 ${failures.length} 项:\n${failures.join('\n')}`);
}

/**
 * 单条 parity 用例的三态执行壳（commands / federation / local 同构）：
 * try 执行 → expect_error / err 分流 → 与黄金基准深比较。
 * pipeline（期望报错文案与 per-key 比较不同）不套本壳；computes / fnfns 无 expect_error 分支走 execGoldenCaseDirect。
 */
function execGoldenCase(fx, g, failures, runCase, wantOf) {
  let actual = null;
  let err = null;
  try {
    actual = runCase(fx);
  } catch (e) {
    err = e;
  }

  if (fx.expect_error) {
    if (!err) failures.push(`[${fx.name}] 期望报错，但绑定未报错: ${fmt(actual)}`);
    return;
  }
  if (err) {
    failures.push(`[${fx.name}] 绑定报错: ${err.message}`);
    return;
  }

  expectDeepEqual(failures, `${fx.name}`, actual, wantOf(g));
}

/** 单条 parity 用例的两态执行壳（computes / fnfns 同构）：报错即记入，否则与黄金基准深比较 */
function execGoldenCaseDirect(fx, g, failures, runCase, wantOf) {
  let actual;
  try {
    actual = runCase(fx);
  } catch (e) {
    failures.push(`[${fx.name}] 绑定报错: ${e.message}`);
    return;
  }
  expectDeepEqual(failures, `${fx.name}`, actual, wantOf(g));
}

/** exists 用例：命令 + 「命中/未命中」与单元素集合视图 */
function planExistsCase(reg, fx) {
  const command = reg.planExists(fx.model || '', fx.condition ?? {});
  return {
    command,
    found: fx.foundDoc != null,
    collections: [command.collection ?? null],
  };
}

/** 只读/单命令分派表（写路径用例不在表内，由 replayWriteCase 兜住） */
const COMMAND_PLANNERS = {
  query: (reg, fx, ctx) => reg.planQuery(fx.gql || '', fx.params || {}, ctx),
  query_with_count: (reg, fx, ctx) => reg.planQueryWithCount(fx.gql || '', fx.params || {}, ctx, fx.total ?? 0),
  resolve_page: (reg, fx) => reg.resolvePage(fx.gql || '', fx.params || {}),
  restore_sort_order: (reg, fx) => reg.restoreSortOrder(fx.items || [], fx.ids || [], fx.sort ?? null),
  insert: (reg, fx, ctx) => planInsertShape(reg, fx, ctx),
  exists: planExistsCase,
  count: (reg, fx) => ({ command: reg.planCount(fx.model || '', fx.filter ?? null) }),
};

/** 按 `kind` 分派，产出与 parity_commands.rs 完全相同的比较结构 */
function runCommandCase(reg, fx) {
  const planner = COMMAND_PLANNERS[fx.kind];
  if (planner) return planner(reg, fx, fx.context ?? null);
  // 写路径用例走 replayWriteCase（对齐 parity_write.rs 的 Host 模拟执行）
  return replayWriteCase(reg, fx);
}

test('core-node parity: commands', () => {
  runParitySuite('commands', path.join('commands', 'expected.json'), (g, fx, failures) => {
    execGoldenCase(fx, g, failures, (fx) => runCommandCase(makeRegistry(fx), fx), goldenWant);
  });
});

// ─── computes ───────────────────────────────────────────────

/** inject_depends 用例：注入信息 + 剥除后的 items（items 非数组 ⇒ stripped=null） */
function computeInjectDepends(reg, fx) {
  const out = reg.injectDepends(fx.gql || '');
  const stripped = Array.isArray(fx.items)
    ? reg.stripDepInjected(out.injectInfo, fx.items).items
    : null;
  return { relDeps: out.relDeps, ast: out.ast, injectInfo: out.injectInfo, stripped };
}

/** computes 分派表：kind → 与 parity_computes.rs 相同的比较结构 */
const COMPUTE_CASES = {
  // 绑定返回 `{doc}`；黄金基准是文档本身
  process_node: (reg, fx, ctx) => reg.processNode(fx.gql || '', fx.doc ?? null, ctx).doc,
  inject_depends: (reg, fx) => computeInjectDepends(reg, fx),
  permission: (reg, fx, ctx) => ({
    canRead: reg.canRead(fx.model, ctx),
    canWrite: reg.canWrite(fx.model, ctx),
    shouldInjectOwner: reg.shouldInjectOwner(fx.model, ctx),
    condition: reg.mergeOwnerCondition(fx.model, ctx, fx.condition ?? null),
    readableFields: reg.readableFields(fx.model, ctx),
    readableRelations: reg.readableRelations(fx.model, ctx),
    writableFields: reg.writableFields(fx.model, ctx),
    filteredData: reg.filterWritableData(fx.model, ctx, fx.data ?? null),
  }),
};

/** 按 `kind` 分派，产出与 parity_computes.rs 完全相同的比较结构 */
function runComputesCase(reg, fx) {
  const executor = COMPUTE_CASES[fx.kind];
  if (!executor) throw new Error(`未知用例类型: ${fx.kind}`);
  return executor(reg, fx, fx.context ?? null);
}

/** 去掉黄金基准里的 `name` / `kind` 元字段，得到纯结果 */
function stripMeta(g) {
  const { name, kind, ...rest } = g;
  return rest;
}

test('core-node parity: computes', () => {
  runParitySuite('computes', path.join('computes', 'expected.json'), (g, fx, failures) => {
    execGoldenCaseDirect(fx, g, failures, (fx) => runComputesCase(makeRegistry(fx), fx), stripMeta);
  });
});

// ─── fnfns（fnRef 回调桥） ──────────────────────────────────

/** postprocess 用例：对齐 crud.query —— 先注入 asyncFn 依赖，再取 postprocess 信息 */
function fnfnsPostprocess(reg, fx, ctx) {
  const inj = reg.injectDepends(fx.gql || '');
  const pp = { ast: inj.ast, inject: inj.injectInfo };
  const prepared = reg.prepareQuery(pp, fx.docs || [], ctx);
  runAsyncFns(prepared.fnRefs, prepared.items, ctx);
  return { items: reg.stripQuery(pp, prepared.items).items };
}

/** fnfns 分派表：kind → 与 parity_fnfns.rs 相同的比较结构 */
const FNFNS_CASES = {
  process_node: (reg, fx, ctx) => reg.processNode(fx.gql || '', fx.doc ?? null, ctx),
  async_fns: (reg, fx, ctx) => {
    const items = fx.items || [];
    runAsyncFns(reg.asyncFnRefs(fx.model, ctx), items, ctx);
    return { items };
  },
  postprocess: (reg, fx, ctx) => fnfnsPostprocess(reg, fx, ctx),
  insert: (reg, fx, ctx) => planInsertShape(reg, fx, ctx),
};

/** 按 `kind` 分派，产出与 parity_fnfns.rs 完全相同的比较结构 */
function runFnfnsCase(reg, fx) {
  const executor = FNFNS_CASES[fx.kind];
  if (!executor) throw new Error(`未知用例类型: ${fx.kind}`);
  return executor(reg, fx, fx.context ?? null);
}

test('core-node parity: fnfns', () => {
  runParitySuite('fnfns', path.join('fnfns', 'expected.json'), (g, fx, failures) => {
    execGoldenCaseDirect(fx, g, failures, (fx) => runFnfnsCase(makeRegistry(fx), fx), goldenWant);
  });
});

// ─── federation（跨库联邦） ─────────────────────────────────

/** 把联邦计划投影成「结构性摘要」（对齐 Rust 侧 project_plan） */
function projectPlan(plan) {
  const sources = (plan.sources || []).map((s) => ({
    key: s.key ?? null,
    source: s.source ?? null,
    database: s.database ?? null,
    schema: s.schema ?? null,
    model: s.model ?? null,
    mode: s.mode ?? null,
  }));
  const edges = (plan.join && plan.join.edges) || [];
  const degraded = (plan.degraded || []).map((d) => d.code).filter((c) => c !== undefined);
  return {
    v: plan.v ?? null,
    kind: plan.kind ?? null,
    root: plan.root ?? null,
    sources,
    edges,
    degraded,
    hasPostprocess: plan.postprocess != null,
  };
}

/** 契约断言：postprocess 必须与单库 planQuery 同形状（不一致即抛，附 want/got） */
function assertPostprocessParity(reg, fx, plan) {
  const single = reg.planQuery(fx.gql || '', fx.params || {}, fx.context ?? null);
  const want = single.postprocess ?? null;
  const got = plan.postprocess ?? null;
  if (isDeepStrictEqual(want, got)) return;
  throw new Error(
    `postprocess 与单库 planQuery 不一致\n    want: ${fmt(want)}\n    got : ${fmt(got)}`,
  );
}

function runFederationPlan(reg, fx) {
  const plan = reg.planFederated(fx.gql || '', fx.params || {}, fx.context ?? null);
  if (fx.parity_with_query) assertPostprocessParity(reg, fx, plan);
  return projectPlan(plan);
}

test('core-node parity: federation', () => {
  runParitySuite('federation', path.join('federation', 'expected.json'), (g, fx, failures) => {
    execGoldenCase(fx, g, failures, (fx) => {
      const reg = makeRegistry(fx);
      return fx.kind === 'plan'
        ? runFederationPlan(reg, fx)
        : reg.mergeFederated(fx.plan ?? null, fx.results || []);
    }, goldenWant);
  });
});

// ─── 同步回调桥的缺失实现 → JS 异常 ────────────────────────

test('core-node: 未注册的 fn 回调报错', () => {
  const reg = new Registry();
  reg.register({
    name: 'Post',
    collection: 'posts',
    timestamps: false,
    fields: { a: { type: 'int' } },
    computes: { total: { type: 'int', fn: true, fnRef: 'missing_fn' } },
    relations: {},
  });
  assert.throws(
    () => reg.processNode('Post{total}', { _id: '1', a: 1 }, null),
    /missing_fn/,
  );
});

// ─── 错误路径：非法 GQL / 未注册 model → 抛出异常（不得返回 {code} 对象） ─

test('core-node: 非法 GQL / 未注册 model 以异常抛出', () => {
  const reg = new Registry();
  assert.throws(
    () => reg.planQuery('Ghost{_id}', {}, null),
    (e) => e instanceof Error && /未注册/.test(e.message),
  );
  assert.throws(
    () => reg.buildPipeline('@@@', {}, null),
    (e) => e instanceof Error && /位置 0/.test(e.message),
  );
});

// ─── local（本地磁盘数据源求值，node ↔ py ↔ MongoDB 驱动语义） ──
//
// 回放 `fixtures/local/{cases,expected}.json`：核心输入来自 core 规划产出的
// Mongo 命令（形状与 mongo 路径同），黄金基准按 MongoDB 驱动语义手写。
// 与 core-py/test/parity_test.py::test_local 共用同一 fixtures —— 两侧都必须
// 与黄金基准逐条深比较，从而保证 node 与 py 绑定输出逐字节相同。

test('core-node parity: local', () => {
  const reg = new Registry();
  runParitySuite('local', path.join('local', 'expected.json'), (g, fx, failures) => {
    execGoldenCase(fx, g, failures, (fx) => reg.localEval(fx.collections, fx.command), (g) => (g.error === true
      ? { error: true }
      : { result: has(g, 'result') ? g.result : null, changed: g.changed, collections: g.collections }));
  });
});

