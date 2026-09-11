'use strict';

/**
 * core-node 绑定 parity 对拍
 *
 * 回放 4 套 fixtures（pipeline / commands / computes / fnfns）与其 JS 黄金基准，
 * 逐条深比较 Rust 绑定层的输出——语义与 core/tests/parity*.rs 完全一致：
 *   - pipeline   : `buildPipeline` 的 tokens / ast / pipeline / projection
 *   - commands   : `planQuery` / `planQueryWithCount` / `resolvePage` /
 *                  `restoreSortOrder` / `planInsert` / `planExists` / `planCount` /
 *                  `planAggregate`
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

const { Registry } = require('../dist/mongo-store-node.node');
const { fns, asyncFns } = require('../../tools/test-fns.js');

const FIXTURES = path.join(__dirname, '..', '..', 'fixtures');

const load = (p) => JSON.parse(fs.readFileSync(p, 'utf8'));
const has = (o, k) => Object.prototype.hasOwnProperty.call(o, k);

/** 建绑定实例并注册 schema / 全部同步 fn 回调（异步回调由 Host 侧直接调用） */
function makeRegistry(fx) {
  const reg = new Registry();
  for (const s of fx.schemas || []) reg.register(s);
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

test('core-node parity: pipeline', () => {
  const cases = load(path.join(FIXTURES, 'pipeline', 'cases.json'));
  const goldens = load(path.join(FIXTURES, 'expected', 'cases.json'));
  assert.equal(cases.length, goldens.length, '输入与黄金基准用例数不一致');

  const failures = [];

  cases.forEach((fx, i) => {
    const g = goldens[i];
    assert.equal(fx.name, g.name, '用例顺序不一致');

    let out = null;
    let err = null;
    try {
      out = makeRegistry(fx).buildPipeline(fx.gql || '', fx.params || {}, fx.context ?? null);
    } catch (e) {
      err = e;
    }

    if (fx.expect_error) {
      if (!err) failures.push(`[${fx.name}] 期望报错但成功返回`);
      return;
    }
    if (err) {
      failures.push(`[${fx.name}] 绑定报错: ${err.message}`);
      return;
    }

    const lines = [];
    for (const k of ['tokens', 'ast', 'pipeline', 'projection']) {
      expectDeepEqual(lines, k, out[k], has(g, k) ? g[k] : null);
    }
    if (lines.length) failures.push(`[${fx.name}] 结果不一致\n${lines.join('\n')}`);
  });

  assert.equal(failures.length, 0, `pipeline 对拍失败 ${failures.length} 项:\n${failures.join('\n')}`);
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

/** 写路径统一回放：Host 模拟执行命令并回喂结果（对齐 parity_write.rs） */
function replayWriteCase(reg, fx) {
  const ctx = fx.context ?? null;
  const now = fx.now ?? 0;
  const updated = fx.updatedDoc ?? null;
  const returnsOf = () => (updated == null ? null : reg.applyWriteDefaults(fx.model, updated));

  switch (fx.kind) {
    case 'insert_many':
      return reg.planInsertMany(fx.model || '', fx.docs || [], now, fx.newIds || [], ctx);

    case 'update': {
      const out = reg.planUpdate(fx.model || '', fx.condition ?? null, fx.data ?? null, fx.options ?? null, now, ctx);
      if (out.needsProbe) throw new Error('unexpected needsProbe');
      return { commands: [out.command], returns: returnsOf() };
    }

    case 'update_many': {
      const out = reg.planUpdateMany(fx.model || '', fx.condition ?? null, fx.data ?? null, now, ctx);
      return { command: out.command, returns: { modifiedCount: fx.modifiedCount ?? 0 } };
    }

    case 'remove': {
      const plan = reg.planRemove(fx.model || '', fx.condition ?? null, ctx);
      const docs = fx.docs || [];
      const commands = [];
      let archivedCount = 0;
      if (plan.findCommand != null) {
        commands.push(plan.findCommand);
        if (docs.length) {
          commands.push(reg.planArchiveDocs(fx.model || '', docs, now).command);
          archivedCount = docs.length;
        }
      }
      commands.push(plan.deleteCommand);
      return { commands, returns: { deletedCount: fx.deletedCount ?? 0, archivedCount } };
    }

    case 'upsert': {
      const out = reg.planUpsert(
        fx.model || '',
        fx.condition ?? null,
        fx.data ?? null,
        fx.options ?? null,
        now,
        (fx.newIds || [''])[0] || '',
        ctx,
      );
      return { command: out.command, returns: returnsOf() };
    }

    case 'mutation': {
      const plan = reg.planMutation(fx.model || '', fx.data ?? null, now, fx.newIds || [], ctx);
      const resolved = [];
      const commands = [];
      let rootResult;
      for (const step of plan.steps) {
        const command = substituteStepPlaceholders(step.command, resolved);
        let result;
        if (command.kind === 'insertOne') result = command.doc ?? null;
        else if (command.kind === 'findOneAndUpdate') result = updated;
        else throw new Error(`未支持的 mutation 步骤命令: ${command.kind}`);
        if (rootResult === undefined) rootResult = result;
        resolved.push(result ? result._id ?? null : null);
        commands.push(command);
      }
      const returns = rootResult == null ? null : reg.applyWriteDefaults(fx.model, rootResult);
      return { commands, returns };
    }

    default:
      return null; // 非 write 用例
  }
}

/** 按 `kind` 分派，产出与 parity_commands.rs 完全相同的比较结构 */
function runCommandCase(reg, fx) {
  const ctx = fx.context ?? null;

  switch (fx.kind) {
    case 'query':
      return reg.planQuery(fx.gql || '', fx.params || {}, ctx);

    case 'query_with_count':
      return reg.planQueryWithCount(fx.gql || '', fx.params || {}, ctx, fx.total ?? 0);

    case 'resolve_page':
      return reg.resolvePage(fx.gql || '', fx.params || {});

    case 'restore_sort_order':
      return reg.restoreSortOrder(fx.items || [], fx.ids || [], fx.sort ?? null);

    case 'insert': {
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

    case 'exists': {
      const command = reg.planExists(fx.model || '', fx.condition ?? {});
      return {
        command,
        found: fx.foundDoc != null,
        collections: [command.collection ?? null],
      };
    }

    case 'count':
      return { command: reg.planCount(fx.model || '', fx.filter ?? null) };

    case 'aggregate':
      return { command: reg.planAggregate(fx.model || '', fx.pipeline || []) };

    default:
      // 写路径用例走 replayWriteCase（对齐 parity_write.rs 的 Host 模拟执行）
      return replayWriteCase(reg, fx);
  }
}

test('core-node parity: commands', () => {
  const cases = load(path.join(FIXTURES, 'commands', 'cases.json'));
  const goldens = load(path.join(FIXTURES, 'commands', 'expected.json'));
  assert.equal(cases.length, goldens.length, '输入与黄金基准用例数不一致');

  const failures = [];

  cases.forEach((fx, i) => {
    const g = goldens[i];
    assert.equal(fx.name, g.name, '用例顺序不一致');

    let actual = null;
    let err = null;
    try {
      actual = runCommandCase(makeRegistry(fx), fx);
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

    const want = g.error === true ? { error: true } : has(g, 'result') ? g.result : null;
    expectDeepEqual(failures, `${fx.name}`, actual, want);
  });

  assert.equal(failures.length, 0, `commands 对拍失败 ${failures.length} 项:\n${failures.join('\n')}`);
});

// ─── computes ───────────────────────────────────────────────

/** 按 `kind` 分派，产出与 parity_computes.rs 完全相同的比较结构 */
function runComputesCase(reg, fx) {
  const ctx = fx.context ?? null;

  switch (fx.kind) {
    case 'process_node':
      // 绑定返回 `{doc}`；黄金基准是文档本身
      return reg.processNode(fx.gql || '', fx.doc ?? null, ctx).doc;

    case 'inject_depends': {
      const out = reg.injectDepends(fx.gql || '');
      const stripped = Array.isArray(fx.items)
        ? reg.stripDepInjected(out.injectInfo, fx.items).items
        : null;
      return { relDeps: out.relDeps, ast: out.ast, injectInfo: out.injectInfo, stripped };
    }

    case 'permission':
      return {
        canRead: reg.canRead(fx.model, ctx),
        canWrite: reg.canWrite(fx.model, ctx),
        shouldInjectOwner: reg.shouldInjectOwner(fx.model, ctx),
        condition: reg.mergeOwnerCondition(fx.model, ctx, fx.condition ?? null),
        readableFields: reg.readableFields(fx.model, ctx),
        readableRelations: reg.readableRelations(fx.model, ctx),
        writableFields: reg.writableFields(fx.model, ctx),
        filteredData: reg.filterWritableData(fx.model, ctx, fx.data ?? null),
      };

    default:
      throw new Error(`未知用例类型: ${fx.kind}`);
  }
}

/** 去掉黄金基准里的 `name` / `kind` 元字段，得到纯结果 */
function stripMeta(g) {
  const { name, kind, ...rest } = g;
  return rest;
}

test('core-node parity: computes', () => {
  const cases = load(path.join(FIXTURES, 'computes', 'cases.json'));
  const goldens = load(path.join(FIXTURES, 'computes', 'expected.json'));
  assert.equal(cases.length, goldens.length, '输入与黄金基准用例数不一致');

  const failures = [];

  cases.forEach((fx, i) => {
    const g = goldens[i];
    assert.equal(fx.name, g.name, '用例顺序不一致');

    let actual;
    try {
      actual = runComputesCase(makeRegistry(fx), fx);
    } catch (e) {
      failures.push(`[${fx.name}] 绑定报错: ${e.message}`);
      return;
    }
    expectDeepEqual(failures, `${fx.name}`, actual, stripMeta(g));
  });

  assert.equal(failures.length, 0, `computes 对拍失败 ${failures.length} 项:\n${failures.join('\n')}`);
});

// ─── fnfns（fnRef 回调桥） ──────────────────────────────────

/** 按 `kind` 分派，产出与 parity_fnfns.rs 完全相同的比较结构 */
function runFnfnsCase(reg, fx) {
  const ctx = fx.context ?? null;

  switch (fx.kind) {
    case 'process_node':
      return reg.processNode(fx.gql || '', fx.doc ?? null, ctx);

    case 'async_fns': {
      const items = fx.items || [];
      runAsyncFns(reg.asyncFnRefs(fx.model, ctx), items, ctx);
      return { items };
    }

    case 'postprocess': {
      // 对齐 crud.query：先注入 asyncFn 依赖，再取 postprocess 信息
      const inj = reg.injectDepends(fx.gql || '');
      const pp = { ast: inj.ast, inject: inj.injectInfo };

      const prepared = reg.prepareQuery(pp, fx.docs || [], ctx);
      runAsyncFns(prepared.fnRefs, prepared.items, ctx);
      return { items: reg.stripQuery(pp, prepared.items).items };
    }

    case 'insert': {
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

    default:
      throw new Error(`未知用例类型: ${fx.kind}`);
  }
}

test('core-node parity: fnfns', () => {
  const cases = load(path.join(FIXTURES, 'fnfns', 'cases.json'));
  const goldens = load(path.join(FIXTURES, 'fnfns', 'expected.json'));
  assert.equal(cases.length, goldens.length, '输入与黄金基准用例数不一致');

  const failures = [];

  cases.forEach((fx, i) => {
    const g = goldens[i];
    assert.equal(fx.name, g.name, '用例顺序不一致');

    let actual;
    try {
      actual = runFnfnsCase(makeRegistry(fx), fx);
    } catch (e) {
      failures.push(`[${fx.name}] 绑定报错: ${e.message}`);
      return;
    }
    const want = g.error === true ? { error: true } : has(g, 'result') ? g.result : null;
    expectDeepEqual(failures, `${fx.name}`, actual, want);
  });

  assert.equal(failures.length, 0, `fnfns 对拍失败 ${failures.length} 项:\n${failures.join('\n')}`);
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
