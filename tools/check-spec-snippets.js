#!/usr/bin/env node
// SPEC 片段一致性校验：本仓所有必含载体归一后逐段一致，否则 exit 1。
// 运行：node tools/check-spec-snippets.js（须在 rust-store/ 根目录执行；root = process.cwd()）
'use strict';
const fs = require('node:fs');
const path = require('node:path');

// 本仓配置：需含各片段的载体 + 取等集合（equality）
const REPO = {
  root: process.cwd(),
  carriers: {
    // 名称 -> { file, ids }；ids='*' 表示需含全部片段
    'core/README.md':     { file: 'core/README.md',     ids: '*' },
    'core-node/README.md': { file: 'core-node/README.md', ids: '*' },
    'core-py/README.md':  { file: 'core-py/README.md',  ids: '*' },
  },
  equality: ['core/README.md', 'core-node/README.md', 'core-py/README.md'],
  ids: ['NAMING-STYLE', 'FNREF'],
};

function norm(t) {
  return t.replace(/\r\n?/g, '\n').split('\n')
    .map((l) => l.trim()).filter((l) => l.length)
    .map((l) => l.replace(/[ \t]+/g, ' ')).join('\n');
}
function extract(text, id) {
  const re = new RegExp(`<!-- SPEC:${id}:BEGIN -->([\\s\\S]*?)<!-- SPEC:${id}:END -->`);
  const m = text.match(re);
  return m ? norm(m[1]) : null;
}

const errors = [];
const seen = {}; // id -> { carrier: normalized }
for (const name of Object.keys(REPO.carriers)) {
  const { file, ids } = REPO.carriers[name];
  const want = ids === '*' ? REPO.ids : ids;
  let text;
  try { text = fs.readFileSync(path.join(REPO.root, file), 'utf8'); }
  catch { errors.push(`缺少载体文件: ${file}`); continue; }
  for (const id of want) {
    const got = extract(text, id);
    if (got === null) { errors.push(`${file} 缺少片段 ${id}`); continue; }
    if (!(id in seen)) seen[id] = {};
    seen[id][name] = got;
  }
}
for (const id of REPO.ids) {
  const members = seen[id] || {};
  const eq = REPO.equality.filter((n) => n in members);
  const ref = members[eq[0]];
  for (const n of eq) {
    if (members[n] !== ref) errors.push(`片段 ${id} 不一致: ${eq[0]} vs ${n}`);
  }
}
if (errors.length) { console.error('SPEC 片段校验失败:\n' + errors.join('\n')); process.exit(1); }
console.log('SPEC 片段校验通过');
