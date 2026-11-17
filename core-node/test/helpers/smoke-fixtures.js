'use strict';

/**
 * dialect 冒烟 / text-to-query 冒烟共用的最小夹具：
 * sql.js WASM 定位、Order + OrderItem 注册（qty 可选）与 sql.js 结果 → 对象行展开。
 */

const path = require('node:path');

/** sql.js WASM 定位（core-node/node_modules 副本，两冒烟文件共用） */
const SQLITE_WASM = path.join(__dirname, '..', '..', 'node_modules', 'sql.js', 'dist', 'sql-wasm.wasm');

/** 注册 Order + OrderItem（一对多 items 关系；t2q 链路不涉及 qty 故可关） */
function registerOrderFamily(reg, { withQty = true } = {}) {
  reg.register({ name: 'Order', collection: 'orders', timestamps: false,
    fields: { code: { type: 'string' }, amount: { type: 'float' } },
    relations: { items: { model: 'OrderItem', type: 'many', localField: '_id', foreignField: 'orderId' } } });
  reg.register({ name: 'OrderItem', collection: 'order_items', timestamps: false,
    fields: withQty
      ? { orderId: { type: 'string' }, sku: { type: 'string' }, qty: { type: 'int' } }
      : { orderId: { type: 'string' }, sku: { type: 'string' } },
    relations: {} });
}

/** sql.js exec 结果（[{columns, values}]）首结果集 → [{col: value}] 对象行数组 */
function toObjectRows(res) {
  const { columns, values } = res[0];
  return values.map((v) => {
    const o = {};
    columns.forEach((c, i) => (o[c] = v[i]));
    return o;
  });
}

module.exports = { SQLITE_WASM, registerOrderFamily, toObjectRows };
