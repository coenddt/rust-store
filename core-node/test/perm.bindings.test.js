'use strict';

/**
 * core-node 权限绑定直测 —— `readableComputes` 导出契约
 *
 * read 白名单计算列的判决函数 core 侧既有（`permission::get_readable_computes`，
 * 与 fields/relations 三函数完全同构），此前绑定面漏导出。本测试锁定绑定面
 * 三态契约（与 `readableFields` 同姿态，三端 parity）：
 *   - `ctx = null/undefined` → `null`（不裁剪，fail-open）；
 *   - 计算列配 `read` 白名单 → 按角色过滤（未命中剔除）；
 *   - 计算列未配 `read` → 全集放行；
 * 出参形态对齐 `readableFields`：sorted_set（排序去重数组）。
 *
 * 运行：node --test test/perm.bindings.test.js
 */

const { test } = require('node:test');
const assert = require('node:assert');
const native = require('./_binding.js');

const CTX_ADMIN = { userId: 'u1', roles: ['admin'] };
const CTX_USER = { userId: 'u2', roles: ['user'] };

function registry() {
  const reg = new native.Registry();
  reg.register({
    name: 'PermDoc',
    collection: 'perm_docs',
    timestamps: false,
    fields: { title: 'string' },
    relations: {},
    computes: {
      adminOnly: { type: 'string', read: ['admin'], fnRef: 'sumAb' },
      staff: { type: 'string', read: ['admin', 'user'], fnRef: 'mulQp' },
      open: { type: 'string', fnRef: 'sumAb' },
    },
  });
  return reg;
}

test('readableComputes: ctx 缺省 → null 不裁剪（与 fields/relations 同姿态）', () => {
  const reg = registry();
  assert.strictEqual(reg.readableComputes('PermDoc', null), null);
  assert.strictEqual(reg.readableComputes('PermDoc'), null);
  assert.strictEqual(reg.readableFields('PermDoc', null), null);
  assert.strictEqual(reg.readableRelations('PermDoc', null), null);
});

test('readableComputes: read 白名单按角色过滤', () => {
  const reg = registry();
  // admin 命中全部白名单 + 未配放行 → 全集（sorted_set）
  assert.deepStrictEqual(reg.readableComputes('PermDoc', CTX_ADMIN), [
    'adminOnly',
    'open',
    'staff',
  ]);
  // user 未命中 adminOnly → 剔除（staff 配了 user 角色仍在）
  assert.deepStrictEqual(reg.readableComputes('PermDoc', CTX_USER), ['open', 'staff']);
});

test('readableComputes: 未配 read → 全集；无 computes → 空数组（非 null）', () => {
  const reg = new native.Registry();
  reg.register({
    name: 'OpenDoc',
    collection: 'open_docs',
    timestamps: false,
    fields: { title: 'string' },
    relations: {},
    computes: {
      b: { type: 'int', fnRef: 'sumAb' },
      a: { type: 'int', fnRef: 'mulQp' },
    },
  });
  assert.deepStrictEqual(reg.readableComputes('OpenDoc', CTX_USER), ['a', 'b']);

  reg.register({
    name: 'BareDoc',
    collection: 'bare_docs',
    timestamps: false,
    fields: { title: 'string' },
    relations: {},
  });
  assert.deepStrictEqual(reg.readableComputes('BareDoc', CTX_USER), []);
});
