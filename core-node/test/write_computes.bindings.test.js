'use strict';

/**
 * core-node `applyWriteComputes` 导出契约
 *
 * 写路径（`findOneAndUpdate` + `returnDocument: AFTER`）返回的落库文档需要叠加
 * `computes` 中的同步 `fn` 计算列；该导出**只叠计算列、不补默认值**（D1：默认值
 * 只在 insert 物化一次，重复填充会让「落库 / returns / 读回」出现不同取值）。
 *
 * 断言覆盖：
 *   1. 无同步 `fn` 声明 ⇒ 原样返回，且不补默认值；
 *   2. 声明 `fn: true` 但未 `setFn` 注册实现 ⇒ 显式报错（不静默跳过）；
 *   3. 落库文档能叠出计算列，且返回值的计算列 == 以同一落库文档读回后的计算列；
 *   4. 入参文档不被改写（返回值是副本）。
 *
 * 运行：node --test test/write_computes.bindings.test.js
 */

const { test } = require('node:test');
const assert = require('node:assert');
const native = require('./_binding.js');

const PLAIN_SCHEMA = {
  name: 'Plain',
  collection: 'plains',
  timestamps: false,
  fields: {
    _id: { type: 'string' },
    a: { type: 'string' },
    status: { type: 'string', default: 'new' },
  },
  relations: {},
};

const ORDER_SCHEMA = {
  name: 'Order',
  collection: 'orders',
  timestamps: false,
  fields: {
    _id: { type: 'string' },
    a: { type: 'string' },
    b: { type: 'string' },
    status: { type: 'string', default: 'new' },
  },
  relations: {},
  computes: {
    total: { type: 'string', fn: true, fnRef: 'total' },
  },
};

test('无 fn 声明：原样返回且不补默认值、不改写入参', () => {
  const reg = new native.Registry();
  reg.register(PLAIN_SCHEMA);

  const doc = { _id: 'p1', a: 'v' };
  const out = reg.applyWriteComputes('Plain', doc);

  assert.deepStrictEqual(out, doc);
  assert.ok(!('status' in out), '不得补默认值（默认值只在 insert 物化一次）');
  assert.deepStrictEqual(doc, { _id: 'p1', a: 'v' }, '入参不得被改写');
});

test('声明 fn:true 但未注册实现：显式报错而非静默跳过', () => {
  const reg = new native.Registry();
  reg.register(ORDER_SCHEMA);

  assert.throws(
    () => reg.applyWriteComputes('Order', { _id: 'o1', a: 'x', b: 'y' }),
    /未注册同步实现/,
  );
});

test('落库文档叠出计算列，且与读回值一致', () => {
  const reg = new native.Registry();
  reg.register(ORDER_SCHEMA);
  reg.setFn('total', (doc) => `${doc.a}-${doc.b}`);

  // update 落库后的文档：只有被更新的字段，没有计算列、没有未参与更新的默认值
  const stored = { _id: 'o1', a: 'x', b: 'y' };
  const returns = reg.applyWriteComputes('Order', stored);

  assert.strictEqual(returns.total, 'x-y', '返回值必须带上计算列');
  assert.deepStrictEqual(stored, { _id: 'o1', a: 'x', b: 'y' }, '计算列不得回写落库文档');

  // 读回路径同样以落库文档为基计算 ⇒ returns 与读回的计算列必须一致（D1 三值归一）
  const readBack = reg.applyWriteComputes('Order', { ...stored });
  assert.strictEqual(returns.total, readBack.total, 'returns 与读回值不得出现两个计算值');

  assert.ok(!('status' in returns), '不得把 insert 默认值补进写路径返回值');
});
