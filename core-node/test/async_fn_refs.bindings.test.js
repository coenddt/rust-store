'use strict';

/**
 * core-node asyncFn 选取绑定契约 —— `asyncFnRefs` / `allAsyncFnKeys`
 *
 * 背景：core 6.0.2 起 asyncFn 计算列按查询**显式请求的字段**执行
 * （`select_async_fns` 新增 `requested`）。绑定层 `asyncFnRefs(model, ctx, fields?)`：
 * `fields` 缺省时仍列出全部（与前版本行为一致，宿主无需改动），给了则与
 * `prepareQuery` 的 `fnRefs` 同口径；`allAsyncFnKeys(model, ctx?)` 是「列出全部键」
 * 的显式入口。
 *
 * 运行：node --test test/async_fn_refs.bindings.test.js
 */

const { test } = require('node:test');
const assert = require('node:assert');
const native = require('./_binding.js');

function registry() {
  const reg = new native.Registry();
  reg.register({
    name: 'Listing',
    collection: 'listings',
    timestamps: false,
    fields: { _id: 'string', title: 'string' },
    relations: {},
    computes: {
      favoriteCount: { type: 'int', asyncFn: true, fnRef: 'live_favorite' },
      views: { type: 'int', asyncFn: true, fnRef: 'live_views' },
    },
  });
  return reg;
}

test('asyncFnRefs 缺省列出全部（向后兼容，宿主无需改动）', () => {
  const reg = registry();
  assert.deepStrictEqual(reg.asyncFnRefs('Listing').sort(), ['live_favorite', 'live_views']);
});

test('asyncFnRefs 传 fields 只返回被请求的计算列', () => {
  const reg = registry();
  assert.deepStrictEqual(reg.asyncFnRefs('Listing', null, ['favoriteCount']), ['live_favorite']);
  assert.deepStrictEqual(
    reg.asyncFnRefs('Listing', null, ['title']),
    [],
    '请求非计算列字段时不应返回任何 fnRef',
  );
});

test('allAsyncFnKeys 返回全部计算列键（不按请求字段过滤）', () => {
  const reg = registry();
  assert.deepStrictEqual(reg.allAsyncFnKeys('Listing').sort(), ['favoriteCount', 'views']);
});
