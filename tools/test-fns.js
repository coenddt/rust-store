'use strict';

/**
 * fnRef 回调桥的测试函数表（JS 侧参考实现）
 *
 * Rust 侧在 core/tests/common/mod.rs 的 TestFnRegistry 有逐一对应的实现，
 * 两侧语义必须完全一致——这是 fnfns 黄金基准的可信度来源。
 *
 * 约定：schema JSON 里计算列声明 `"fn": true` / `"asyncFn": true`（可选 `"fnRef"`），
 * 注册前由 patchFns 把声明替换为本表中的真实函数。
 */

const fns = {
  sum_ab: (doc) => (doc.a || 0) + (doc.b || 0),
  mul_qp: (doc) => (doc.qty || 0) * (doc.price || 0),
  label: (doc) => (doc.v > 10 ? 'big' : 'small'),
  null_fn: () => null,
  greet: (doc) => 'hi:' + (doc.name || ''),
};

const asyncFns = {
  fill_area: (items) => {
    for (const it of items) it.area = (it.w || 0) * (it.h || 0);
  },
  ctx_tag: (items, ctx) => {
    const t = ctx && ctx.userId ? 'u:' + ctx.userId : 'anon';
    for (const it of items) it.tag = t;
  },
  tag_skus: (items) => {
    for (const it of items) {
      it.skuTag = (it.items || []).map((s) => s.sku).join('|');
    }
  },
};

/** 注册 schema 前：把 computes 里的 fn/asyncFn 声明替换为真实函数（fnRef 缺省 = key 名） */
function patchFns(defn) {
  for (const [key, comp] of Object.entries(defn.computes || {})) {
    const ref = comp.fnRef || key;
    if (comp.fn) comp.fn = fns[ref];
    if (comp.asyncFn) comp.asyncFn = asyncFns[ref];
  }
  return defn;
}

module.exports = { fns, asyncFns, patchFns };
