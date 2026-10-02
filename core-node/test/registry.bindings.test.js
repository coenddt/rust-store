'use strict';

/**
 * core-node Registry 注册表生命周期绑定直测 —— `clearSchemas` 导出契约
 *
 * 进程级 schema 注册表的**测试隔离 / 动态重建**原语：清空 schemas + 注册顺序，
 * 不动 requireContext / profile 配置开关（与 clearFns「各清各的」对称）。
 * 三端 parity：core Rust 测试 profile.rs::registry_clear_schemas_but_keeps_switches、
 * core-py test/registry_bindings_test.py 同构断言。
 *
 * 运行：node --test test/registry.bindings.test.js
 */

const { test } = require('node:test');
const assert = require('node:assert');
const native = require('./_binding.js');

const DEFN = {
  name: 'C',
  collection: 'cs',
  timestamps: false,
  fields: { a: 'string' },
  relations: {},
};

test('clearSchemas: 清空 schema 与注册顺序，保留配置开关', () => {
  const reg = new native.Registry();
  reg.register(DEFN);
  assert.ok(reg.has('C'));
  assert.deepStrictEqual(reg.list(), ['C', 'CDeleted']);

  reg.setProfile('text2query');
  reg.setRequireContext(true);

  reg.clearSchemas();
  assert.ok(!reg.has('C'));
  assert.deepStrictEqual(reg.list(), []);
  assert.strictEqual(reg.profile(), 'text2query');
  assert.strictEqual(reg.requireContext(), true);

  reg.register(DEFN);
  assert.ok(reg.has('C'));
});
