'use strict';

/**
 * core-node RBAC 绑定直测 —— setRbac 注入 + 查询面 + 与 plan 链路联动
 *
 * 三端 parity 基准（对齐 core-py/test/rbac_bindings_test.py，core-ffi 同断言）：
 *   1. enforce 受管角色：granted 动作 rbacCan=true，未授予动作 false；
 *   2. enforce 受管角色 + 未配置 model → default deny（false）；
 *   3. rbacReadableFields 只返回 readFields ∩ 静态可读（+_id 豁免）；
 *   4. planQuery 产命令在 viewer ctx 下投影被裁（判决在 plan 链路生效）；
 *   5. ownerOnly grant 的 query 命令 filter 含 createdBy = userId；
 *   6. setRbac(null) 后全部恢复直通。
 *
 * 运行：node --test test/rbac.bindings.test.js
 * （前置：npx napi build --platform 产出绑定 .node）
 */

const { test } = require('node:test');
const assert = require('node:assert');
const native = require('./_binding.js');

const POLICY = {
  mode: 'enforce',
  roles: { viewer: {}, editor: {} },
  grants: [
    { role: 'viewer', model: 'Post', actions: ['read'], readFields: ['title'] },
    { role: 'editor', model: 'Post', actions: ['read', 'insert'] },
    { role: 'editor', model: 'Comment', actions: ['read'], ownerOnly: true },
  ],
};
const CTX_EDITOR = { userId: 'u1', roles: ['editor'] };
const CTX_VIEWER = { userId: 'u2', roles: ['viewer'] };

function makeRegistry(policy = POLICY) {
  const reg = new native.Registry();
  for (const defn of [
    {
      name: 'Post',
      collection: 'posts',
      timestamps: false,
      fields: { title: 'string', body: 'string', secret: 'string' },
      relations: {},
    },
    {
      name: 'Comment',
      collection: 'comments',
      timestamps: false,
      fields: { body: 'string' },
      relations: {},
    },
    {
      name: 'User',
      collection: 'users',
      timestamps: false,
      fields: { name: 'string' },
      relations: {},
    },
  ]) {
    reg.register(defn);
  }
  if (policy !== null) reg.setRbac(policy);
  return reg;
}

/** 提取首命令投影键（find 的 projection 键 / aggregate 的 $project 阶段） */
function projectionKeys(plan) {
  const cmd = (plan.commands || [])[0] || {};
  if (cmd.projection && typeof cmd.projection === 'object') {
    return Object.keys(cmd.projection);
  }
  for (const stage of cmd.pipeline || []) {
    if (stage.$project) return Object.keys(stage.$project);
  }
  return [];
}

test('rbacCan grant and deny by action', () => {
  const reg = makeRegistry();
  assert.strictEqual(reg.rbacCan('Post', 'insert', CTX_EDITOR), true);
  assert.strictEqual(reg.rbacCan('Post', 'read', CTX_EDITOR), true);
  assert.strictEqual(reg.rbacCan('Post', 'remove', CTX_EDITOR), false);
});

test('rbacCan default denies unconfigured model (enforce)', () => {
  const reg = makeRegistry();
  assert.strictEqual(reg.rbacCan('User', 'read', CTX_EDITOR), false);
});

test('rbacReadableFields intersects readFields', () => {
  const reg = makeRegistry();
  const fields = reg.rbacReadableFields('Post', CTX_VIEWER);
  // 与静态 readable_fields 同语义（遍历 schema.fields，不含 _id；投影侧 _id 豁免是
  // build_projection 的独立行为）
  assert.deepStrictEqual([...fields].sort(), ['title']);
  // editor 无 readFields 声明 → 字段维度不收紧（base 集合原样；null 仅在 ctx=None）
  assert.deepStrictEqual(
    [...reg.rbacReadableFields('Post', CTX_EDITOR)].sort(),
    ['body', 'secret', 'title'],
  );
  assert.strictEqual(reg.rbacReadableFields('Post', null), null);
});

test('planQuery projection trimmed by rbac', () => {
  const reg = makeRegistry();
  const plan = reg.planQuery('Post{ title body secret }', {}, CTX_VIEWER);
  const keys = projectionKeys(plan);
  assert.ok(!keys.includes('secret'), `viewer 不得投影 secret: ${keys}`);
  assert.ok(keys.includes('title'), `可读字段应保留: ${keys}`);
});

test('ownerOnly query injects createdBy', () => {
  const reg = makeRegistry();
  const plan = reg.planQuery('Comment{ body }', {}, CTX_EDITOR);
  const s = JSON.stringify(plan);
  assert.ok(s.includes('createdBy'), `ownerOnly 读应注入 createdBy 条件: ${s}`);
  assert.ok(s.includes('u1'), `行条件应绑定 userId: ${s}`);
});

test('clear policy restores passthrough', () => {
  const reg = makeRegistry();
  assert.strictEqual(reg.rbacCan('User', 'read', CTX_EDITOR), false);
  assert.strictEqual(reg.rbacEnabled(), true);
  reg.setRbac(null);
  assert.strictEqual(reg.rbacEnabled(), false);
  assert.strictEqual(reg.rbacCan('User', 'read', CTX_EDITOR), true);
});

test('invalid action rejected explicitly', () => {
  const reg = makeRegistry();
  assert.throws(() => reg.rbacCan('Post', 'drop', CTX_EDITOR), /drop/);
});
