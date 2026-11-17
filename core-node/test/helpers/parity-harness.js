'use strict';

/**
 * core-node parity 对拍的公共骨架（D4-4 零语义拆分）。
 *
 * 自 test/parity.test.js 原样下沉：fixtures 装载、绑定实例构造与同步 fn 回调注册、
 * 深比较收集器、套件统一骨架与单条用例执行壳、黄金基准期望口径、planInsert 比较结构；
 * 语句与注释逐字未改。
 *
 * 放在 test/helpers 下（与 smoke-fixtures.js 同惯例）：package.json 的 test 脚本
 * 显式列出用例文件，helpers 不参与 node --test 收集，不产生计数漂移。
 */

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { isDeepStrictEqual } = require('node:util');

const { Registry } = require('../_binding');
const { fns, asyncFns } = require('../../../tools/test-fns.js');

// 自 test/ 下沉到 test/helpers/ 后多一层（__dirname 指向 core-node/test/helpers）
const FIXTURES = path.join(__dirname, '..', '..', '..', 'fixtures');

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

module.exports = {
  load, has, makeRegistry, runAsyncFns, fmt, expectDeepEqual,
  runParitySuite, execGoldenCase, execGoldenCaseDirect, goldenWant, planInsertShape,
};
