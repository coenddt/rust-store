'use strict';

/**
 * 绑定产物解析（测试共用）
 *
 * 同一份 parity 测试要在两种产物布局下都能跑：
 *   1. 开发期约定：`core-node/dist/rust-store-node.node`
 *      （见 nodejs-store/src/core.js 的 LOCAL_CORE 兜底路径与 .gitignore）；
 *   2. `napi build --platform` 的默认输出：包根目录 `rust-store-node.<triple>.node`
 *      （CI `node-binding` 任务即走此路径，此前因只认 dist/ 而恒报 MODULE_NOT_FOUND）。
 *
 * 未找到时显式报错并给出构建命令，不静默降级。
 */

const fs = require('node:fs');
const path = require('node:path');

const ROOT = path.join(__dirname, '..');

function _load() {
  const distFile = path.join(ROOT, 'dist', 'rust-store-node.node');
  if (fs.existsSync(distFile)) return require(distFile);

  const triples = fs
    .readdirSync(ROOT)
    .filter((f) => /^rust-store-node\..+\.node$/.test(f))
    .sort();
  if (triples.length) return require(path.join(ROOT, triples[0]));

  throw new Error(
    '未找到 rust-store-node 绑定产物。\n'
    + '请先构建：npx napi build --platform（见 package.json build:debug）\n'
    + `已查找：${distFile} 与 ${ROOT}/rust-store-node.<triple>.node`,
  );
}

module.exports = _load();
