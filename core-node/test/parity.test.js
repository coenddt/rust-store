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
 *
 * D4-4 零语义拆分：公共骨架（fixtures 装载 / makeRegistry / 深比较收集器 / 套件壳 /
 * goldenWant / planInsertShape）下沉 test/helpers/parity-harness.js，commands 段的
 * 命令回放器（占位符替换 / 写路径回放 / 分派表）下沉 test/helpers/parity-replay.js；
 * 用例名、断言与注释逐字未改。
 */

const { test } = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const { isDeepStrictEqual } = require('node:util');

const { Registry } = require('./_binding');
const {
  has, makeRegistry, runAsyncFns, fmt, expectDeepEqual,
  runParitySuite, execGoldenCase, execGoldenCaseDirect, goldenWant, planInsertShape,
} = require('./helpers/parity-harness');
const { runCommandCase } = require('./helpers/parity-replay');

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
