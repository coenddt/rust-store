'use strict';

/**
 * 黄金基准复算校验器（冻结快照的可复现守卫）
 *
 * 背景：`fixtures/<set>/expected*.json` 是重构前**单体 JS 参考实现**产出的黄金基准。
 * 该 JS 实现已随重构退役（原目录已不存在），快照**无源可再生**，因此本仓库
 * **不再提供 `gen-*-fixtures.js` 生成器**——任何「从当前 core 反向生成」都只是
 * core 与自身比对，会把「与原 JS 的语义漂移」伪装成通过。
 *
 * 本脚本提供替代保障：用当前三侧实现（Rust core / core-node 绑定 / core-py 绑定）
 * 各自逐条重算 `fixtures/<set>/cases.json`，并与冻结黄金基准深比较；三侧全绿即视为
 * 「无 diff、可复现」。逐条差异由各侧测试自身报告（schema 不一致会打印 want/got）。
 *
 * 覆盖的 fixtures：pipeline / commands / computes / fnfns / federation
 * （dialect 无外部 golden，输入内联于 `parity_dialect.rs`，随 Rust core 侧一并复算）。
 *
 * 运行：node tools/verify-fixtures.js
 * 退出码：0 = 三侧全部一致；非 0 = 存在 diff。
 */

const { spawnSync } = require('node:child_process');
const path = require('node:path');

const ROOT = path.join(__dirname, '..'); // rust-store/

const SUITES = [
  {
    name: 'Rust core',
    cwd: ROOT,
    cmd: 'cargo test -p rust-store-core',
    env: {},
  },
  {
    name: 'core-node 绑定',
    cwd: path.join(ROOT, 'core-node'),
    cmd: 'node --test test/parity.test.js',
    env: {},
  },
  {
    name: 'core-py 绑定',
    cwd: ROOT,
    cmd: 'python -m pytest core-py/test/parity_test.py -q -p no:cacheprovider',
    env: { LOCAL_CORE: '1' },
  },
];

function run(suite) {
  const res = spawnSync(suite.cmd, {
    cwd: suite.cwd,
    shell: true,
    env: { ...process.env, ...suite.env },
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
  });
  return { code: res.status ?? 1, out: `${res.stdout || ''}${res.stderr || ''}` };
}

console.log('黄金基准复算校验（fixtures = 冻结 JS 快照，只读）');
console.log(`fixtures：${path.join(ROOT, 'fixtures')}\n`);

let failed = 0;
for (const suite of SUITES) {
  process.stdout.write(`· ${suite.name} … `);
  const { code, out } = run(suite);
  if (code === 0) {
    console.log('一致');
    continue;
  }
  failed += 1;
  console.log('不一致');
  const lines = out.split('\n').filter((l) => l.trim());
  console.log(lines.slice(-40).map((l) => `    ${l}`).join('\n'));
}

if (failed) {
  console.error(`\n复算校验失败：${failed}/${SUITES.length} 侧与冻结黄金基准不一致。`);
  process.exit(1);
}
console.log(`\n复算校验通过：${SUITES.length}/${SUITES.length} 侧与冻结黄金基准完全一致。`);
