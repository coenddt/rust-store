'use strict';

/**
 * text-to-query skill 端到端验证（两阶段 GQL 聚合）
 *   GQL → planQuery(两阶段) → ① phase1(取排序后的 _id) → 替换 {{phase1.ids}}
 *   → ② phase2($lookup + $project) → dialectTranslate(SQLite) → sql.js 执行 → restoreRows
 *
 * 验证 skill 产出的 GQL+params 真实可落地到 SQLite 并还原出嵌套文档。
 */
const initSqlJs = require('sql.js');
const path = require('node:path');
const { Registry } = require('../dist/rust-store-node.node');

const SQLITE_WASM = path.join(__dirname, '..', 'node_modules', 'sql.js', 'dist', 'sql-wasm.wasm');

/** 执行 translate 产出，返回 { rows, shape } */
function execute(db, reg, out) {
  const stmt = out.stmts[0];
  const res = db.exec(stmt.text, stmt.params);
  const { columns, values } = res[0];
  const rows = values.map((v) => { const o = {}; columns.forEach((c, i) => (o[c] = v[i])); return o; });
  return { rows, shape: stmt.rowShape };
}

(async () => {
  const SQL = await initSqlJs({ locateFile: () => SQLITE_WASM });
  const reg = new Registry();

  reg.register({ name: 'Order', collection: 'orders', timestamps: false,
    fields: { code: { type: 'string' }, amount: { type: 'float' } },
    relations: { items: { model: 'OrderItem', type: 'many', localField: '_id', foreignField: 'orderId' } } });
  reg.register({ name: 'OrderItem', collection: 'order_items', timestamps: false,
    fields: { orderId: { type: 'string' }, sku: { type: 'string' } },
    relations: {} });

  // ── skill 产出 ────────────────────────────────────────
  const gql = 'Order($condition:@c0,$sort:@s0,$limit:@l){ code, amount, items{ sku } }';
  const params = { c0: { amount: { $gt: 10 } }, s0: { amount: -1 }, l: 20 };
  console.log('GQL   :', gql);
  console.log('params:', JSON.stringify(params));

  const plan = reg.planQuery(gql, params, null);
  console.log('\n两阶段命令数:', plan.commands.length);

  // ── 建库 + 造数 ───────────────────────────────────────
  const db = new SQL.Database();
  db.run(`CREATE TABLE orders (_id TEXT PRIMARY KEY, code TEXT, amount REAL)`);
  db.run(`CREATE TABLE order_items (_id TEXT PRIMARY KEY, orderId TEXT, sku TEXT)`);
  db.run(`INSERT INTO orders VALUES ('o1','A',50),('o2','B',3),('o3','C',90),('o4','D',70)`);
  db.run(`INSERT INTO order_items VALUES ('i1','o1','s-x'),('i2','o1','s-y'),('i3','o3','s-z')`);

  // ── phase1：排序 + 取 _id ─────────────────────────────
  const p1 = reg.dialectTranslate('sqlite', plan.commands[0]);
  console.log('\n[phase1] SQL:', p1.stmts[0].text);
  const first = execute(db, reg, p1);
  const ids = first.rows.map((r) => r._id);
  console.log('[phase1] _ids:', JSON.stringify(ids));
  if (ids.join(',') !== 'o3,o4,o1') throw new Error(`phase1 顺序应 o3,o4,o1（amount 降序且 >10），实得 ${ids}`);

  // ── 替换 {{phase1.ids}} 进 phase2 ─────────────────────
  const phase2 = JSON.parse(JSON.stringify(plan.commands[1]));
  const mIn = phase2.pipeline[0].$match._id.$in;
  if (mIn !== '{{phase1.ids}}') throw new Error(`phase2 占位符异常: ${mIn}`);
  phase2.pipeline[0].$match._id.$in = ids;

  const p2 = reg.dialectTranslate('sqlite', phase2);
  console.log('\n[phase2] SQL:', p2.stmts[0].text);
  const second = execute(db, reg, p2);
  console.log('[phase2] rows:', JSON.stringify(second.rows));

  const docs = reg.restoreRows(second.shape, second.rows);

  // 两阶段查询：按 phase-1 的 _id 顺序恢复输出序（文档「restore_sort_order」）
  const ordered = ids.map((id) => docs.find((d) => d._id === id)).filter(Boolean);
  console.log('\nrestored(按 phase1 排序):', JSON.stringify(ordered, null, 1));

  const byCode = Object.fromEntries(ordered.map((d) => [d.code, d]));
  if (!byCode.A || !byCode.C || !byCode.D) throw new Error('应含 A/C/D');
  if (byCode.B) throw new Error('B amount=3 不应命中');
  if (byCode.A.items.map((i) => i.sku).sort().join(',') !== 's-x,s-y') throw new Error('A 明细 sku 错');
  if (byCode.C.items[0].sku !== 's-z') throw new Error('C 明细 sku 错');
  if (byCode.D.items.length !== 0) throw new Error('D 无明细应为空数组');
  if (ordered[0].code !== 'C') throw new Error('按 amount 降序首条应为 C');
  console.log('\n✅ text-to-query 产出在 SQLite 端到端跑通：filter+sort+limit + $lookup(many) → 嵌套文档还原');
})();