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
 *   7. 豁免清单（清单化语义）：默认空清单 admin 受管即拒；setExemptRoles 后直通；清除恢复；
 *   8. 拒写清单 / 未配置姿态透传：denyWriteRoles 拒写不拒读；unconfiguredPolicy=closed 全拒。
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

// ─── 7. 豁免清单组（清单化语义——设计 §11.3/§11.5） ─────────────────

const EXEMPT_POLICY = {
  mode: 'enforce',
  roles: { admin: {} }, // admin 显式受管 → 默认无豁免时吃 default deny
  grants: [],
};

test('exempt roles configurable (default deny, inject, clear)', () => {
  const reg = makeRegistry(EXEMPT_POLICY);
  const ctxAdmin = { userId: 'boss', roles: ['admin'] };
  // 默认空清单：受管角色 + 无 grant → 无例外 default deny（rbacCan=false、plan 链路拒）
  assert.strictEqual(reg.rbacCan('Post', 'read', ctxAdmin), false);
  assert.throws(() => reg.planQuery('Post{ title }', {}, ctxAdmin));
  // setExemptRoles 注入 → 直通（静态白名单与 RBAC 判决双直通）
  reg.setExemptRoles(['admin']);
  assert.strictEqual(reg.rbacCan('Post', 'read', ctxAdmin), true);
  reg.planQuery('Post{ title }', {}, ctxAdmin);
  // 清空恢复 deny
  reg.setExemptRoles([]);
  assert.strictEqual(reg.rbacCan('Post', 'read', ctxAdmin), false);
});

// ─── 8. 拒写清单 / 未配置姿态透传组 ─────────────────────────────────

test('deny write roles and unconfigured policy passthrough', () => {
  const reg = makeRegistry(POLICY); // 复用文件头既有 enforce 策略（viewer/editor 受管）
  const ctxViewer = { userId: 'u2', roles: ['viewer'] };
  const ctxStranger = { userId: 'u9', roles: ['stranger'] };
  // 拒写清单（canWrite 走静态 can_write_schema——denyWriteRoles 的判决落点）：
  // 默认空清单 → 写放行；注入后拒写不拒读（读不受影响语义）
  assert.strictEqual(reg.canWrite('Post', ctxViewer), true);
  reg.setDenyWriteRoles(['viewer']);
  assert.strictEqual(reg.canWrite('Post', ctxViewer), false);
  assert.strictEqual(reg.canRead('Post', ctxViewer), true); // 读不受影响
  reg.setDenyWriteRoles([]);
  assert.strictEqual(reg.canWrite('Post', ctxViewer), true);
  // 未配置姿态 closed：stranger（未受管、无豁免）对未配置 model（User）读拒（fail-secure）
  reg.setUnconfiguredPolicy('closed');
  assert.throws(() => reg.planQuery('User{ name }', {}, ctxStranger));
  // 豁免直通先于姿态（设计 §11.3「一切判决环节直通」）：豁免 ctx 在 closed 下仍放行
  reg.setExemptRoles(['stranger']);
  reg.planQuery('User{ name }', {}, ctxStranger);
  reg.setUnconfiguredPolicy('open');
  reg.planQuery('User{ name }', {}, { userId: 'u8', roles: ['wanderer'] });
});
