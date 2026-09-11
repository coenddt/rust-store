'use strict';

/**
 * dialect 端到端冒烟测试：Mongo 命令 → SQL → sql.js(SQLite) 执行 → 嵌套文档还原
 *
 * 验证整条链路（对应「JS Host：SQLite 执行器 + 同步流 + 端到端」里程碑的最小闭环）：
 *   Registry.register(schemaJSON)
 *     → Registry.dialectTranslate(backend, command) : { stmts: [{text, params, rowShape}] }
 *     → sql.js 执行参数化语句
 *     → Registry.restoreRows(rowShape, rows) : 嵌套 Mongo 文档数组
 *
 * 运行：node --test core-node/test/dialect.smoke.test.js
 */

const { test, before } = require('node:test');
const assert = require('node:assert/strict');
const initSqlJs = require('sql.js');
const path = require('node:path');

const { Registry } = require('../dist/mongo-store-node.node');

const SQLITE_WASM = path.join(
  __dirname,
  '..',
  'node_modules',
  'sql.js',
  'dist',
  'sql-wasm.wasm',
);

let SQL;
let reg = new Registry();

before(async () => {
  SQL = await initSqlJs({ locateFile: () => SQLITE_WASM });
  // 注册 schema：posts 标量表 + orders/order_items 一对多关系
  reg.register({ name: 'Post', collection: 'posts', timestamps: false,
    fields: { title: { type: 'string' }, status: { type: 'string' }, views: { type: 'int' } },
    relations: {} });
  reg.register({ name: 'Order', collection: 'orders', timestamps: false,
    fields: { code: { type: 'string' }, amount: { type: 'float' } },
    relations: { items: { model: 'OrderItem', type: 'many', localField: '_id', foreignField: 'orderId' } } });
  reg.register({ name: 'OrderItem', collection: 'order_items', timestamps: false,
    fields: { orderId: { type: 'string' }, sku: { type: 'string' }, qty: { type: 'int' } },
    relations: {} });
});

function db() {
  return new SQL.Database();
}

/** 执行 translate 产出的全部语句，返回最后一条 SELECT 的还原文档 */
function runAll(sqlDb, out) {
  let lastShape = null;
  let lastRows = null;
  for (const stmt of out.stmts) {
    if (stmt.rowShape) lastShape = stmt.rowShape;
    const res = sqlDb.exec(stmt.text, stmt.params);
    if (stmt.rowShape) lastRows = res;
  }
  if (!lastShape || !lastRows || lastRows.length === 0) return [];
  // 把 columns/values 展开成 [{col:value}] 对象行，再交给 restoreRows
  const { columns, values } = lastRows[0];
  const rows = values.map((v) => {
    const o = {};
    columns.forEach((c, i) => (o[c] = v[i]));
    return o;
  });
  return reg.restoreRows(lastShape, rows);
}

test('dialect smoke: insert + find roundtrip on sqlite', () => {
  const sqlDb = db();
  sqlDb.run(`CREATE TABLE posts (_id TEXT PRIMARY KEY, title TEXT, status TEXT, views INTEGER)`);

  const insert = reg.dialectTranslate('sqlite', {
    kind: 'insertOne', collection: 'posts',
    doc: { _id: 'p1', title: '你好', status: 'draft', views: 5 },
  });
  runAll(sqlDb, insert);

  const find = reg.dialectTranslate('sqlite', {
    kind: 'find', collection: 'posts', filter: { status: 'draft' }, projection: { _id: 1, title: 1, status: 1, views: 1 },
  });
  const docs = runAll(sqlDb, find);
  assert.equal(docs.length, 1);
  assert.equal(docs[0].title, '你好');
  assert.equal(docs[0].views, 5);
  assert.ok(docs[0]._id === 'p1', '应还原 _id');
});

test('dialect smoke: insertMany + count on sqlite', () => {
  const sqlDb = db();
  sqlDb.run(`CREATE TABLE posts (_id TEXT PRIMARY KEY, title TEXT, status TEXT, views INTEGER)`);
  runAll(sqlDb, reg.dialectTranslate('sqlite', {
    kind: 'insertMany', collection: 'posts',
    docs: [
      { _id: 'a', title: 'A', status: 'draft', views: 1 },
      { _id: 'b', title: 'B', status: 'draft', views: 2 },
    ],
  }));
  const count = runAll(sqlDb, reg.dialectTranslate('sqlite', {
    kind: 'countDocuments', collection: 'posts', filter: { views: { $gte: 2 } },
  }));
  // count 语句 rowShape 为空，runAll 返回 []
  const n = sqlDb.exec('SELECT COUNT(*) AS n FROM posts WHERE views >= ?', [2]);
  assert.equal(n[0].values[0][0], 1);
});

test('dialect smoke: $lookup join → nested items array', () => {
  const sqlDb = db();
  sqlDb.run(`CREATE TABLE orders (_id TEXT PRIMARY KEY, code TEXT, amount REAL)`);
  sqlDb.run(`CREATE TABLE order_items (_id TEXT PRIMARY KEY, orderId TEXT, sku TEXT, qty INTEGER)`);
  sqlDb.run(`INSERT INTO orders VALUES ('o1','A-1',10),('o2','B-1',20)`);
  sqlDb.run(`INSERT INTO order_items VALUES ('i1','o1','sku-x',2),('i2','o1','sku-y',3)`);

  const agg = reg.dialectTranslate('sqlite', {
    kind: 'aggregate', collection: 'orders', pipeline: [
      { $match: { amount: { $gt: 0 } } },
      { $lookup: { from: 'order_items', localField: '_id', foreignField: 'orderId', as: 'items' } },
    ],
  });
  const docs = runAll(sqlDb, agg);

  assert.equal(docs.length, 2);
  const o1 = docs.find((d) => d._id === 'o1');
  assert.equal(o1 && o1.items.length, 2, 'o1 应有 2 个 items');
  assert.deepEqual(
    o1.items.map((i) => i.sku).sort(),
    ['sku-x', 'sku-y'],
  );
  const o2 = docs.find((d) => d._id === 'o2');
  assert.deepEqual(o2.items, [], 'o2 无匹配子行应为空数组');
});