#!/usr/bin/env node
// npm dist-tag 守护：回发旧版本（back-port）绝不允许把 latest 指针降级。
//
// npm publish 未显式指定 --tag 时会把 latest 指到「本次发布的版本」，于是补发
// v5.3.4 这类旧 tag 会把 latest 从 6.0.1 抢到 5.3.4，用户 `npm i rust-store-node`
// 就会静默装到旧版。本脚本提供两个能力：
//
//   node scripts/npm-dist-tag.mjs resolve <package> <version>
//     待发版本高于当前 latest ⇒ 打印 latest；不高于、或读不到 latest ⇒ 打印 legacy。
//     读不到 latest 时取 legacy：宁可 latest 滞后（一条命令可手动修正），也不静默降级。
//
//   node scripts/npm-dist-tag.mjs repair <package> [version]
//     把 latest 指到 <version>，未给则指到已发布的最高稳定版本（忽略 prerelease）。
//     已指向目标时跳过。发布失败以非零退出，不静默吞掉。
//
// 版本比较按 semver 口径：1.0.0 > 1.0.0-rc.1；prerelease 逐段字符串比较（够用且无依赖）。

import { execFileSync } from 'node:child_process'

const npm = (args) => {
  try {
    // Windows 上 npm 是 npm.cmd，execFile 必须经 shell 才能找到；参数只含包名/版本，无注入面。
    return execFileSync('npm', args, {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      shell: process.platform === 'win32',
    }).trim()
  } catch (err) {
    // 包不存在 / 无权限 / 网络失败都落到这里；由调用方决定是保守降级还是直接失败。
    return null
  }
}

const parseSemver = (v) => {
  const m = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(
    String(v).trim(),
  )
  if (!m) return null
  return { core: [Number(m[1]), Number(m[2]), Number(m[3])], pre: m[4] || '' }
}

// 返回 1 / 0 / -1；任一版本不合法返回 null。
const compareSemver = (a, b) => {
  const x = parseSemver(a)
  const y = parseSemver(b)
  if (!x || !y) return null
  for (let i = 0; i < 3; i++) {
    if (x.core[i] !== y.core[i]) return x.core[i] > y.core[i] ? 1 : -1
  }
  if (!x.pre && y.pre) return 1
  if (x.pre && !y.pre) return -1
  if (x.pre === y.pre) return 0
  return x.pre > y.pre ? 1 : -1
}

const currentLatest = (pkg) => npm(['view', pkg, 'dist-tags.latest'])

const resolveCmd = (pkg, version) => {
  const latest = currentLatest(pkg)
  if (!latest) {
    console.log('legacy')
    process.stderr.write(
      `::warning::读不到 ${pkg} 的当前 latest，本次发布按 legacy 处理（不推进 latest）\n`,
    )
    return 0
  }
  const c = compareSemver(version, latest)
  if (c === null) {
    console.log('legacy')
    process.stderr.write(
      `::warning::版本无法按 semver 解析（version=${version} latest=${latest}），按 legacy 处理\n`,
    )
    return 0
  }
  console.log(c > 0 ? 'latest' : 'legacy')
  if (c <= 0) {
    process.stderr.write(
      `::warning::待发版本 ${version} 不高于 ${pkg} 当前 latest ${latest}，以 --tag legacy 发布，latest 保持不动\n`,
    )
  }
  return 0
}

const allVersions = (pkg) => {
  const raw = npm(['view', pkg, 'versions', '--json'])
  if (!raw) return null
  try {
    const d = JSON.parse(raw)
    return Array.isArray(d) ? d : [d]
  } catch {
    return null
  }
}

const highestStable = (pkg) => {
  const vs = allVersions(pkg)
  if (!vs) return null
  const stable = vs.filter((v) => parseSemver(v) && !parseSemver(v).pre)
  if (stable.length === 0) return null
  return stable.reduce((acc, v) => (compareSemver(v, acc) > 0 ? v : acc))
}

const repairCmd = (pkg, version, dryRun) => {
  const target = version || highestStable(pkg)
  if (!target) {
    process.stderr.write(`::error::无法确定 ${pkg} 的目标版本（版本列表不可读或全为 prerelease）\n`)
    return 1
  }
  if (parseSemver(target) === null) {
    process.stderr.write(`::error::目标版本 ${target} 不是合法 semver\n`)
    return 1
  }
  const latest = currentLatest(pkg)
  if (latest === target) {
    console.log(`${pkg}: latest 已是 ${target}，跳过`)
    return 0
  }
  if (dryRun) {
    console.log(`${pkg}: latest ${latest ?? '<unknown>'} → ${target}（dry-run，未执行）`)
    return 0
  }
  const out = npm(['dist-tag', 'add', `${pkg}@${target}`, 'latest'])
  if (out === null) {
    process.stderr.write(`::error::npm dist-tag add ${pkg}@${target} latest 失败（看上方 npm 输出）\n`)
    return 1
  }
  console.log(`${pkg}: latest ${latest ?? '<unknown>'} → ${target}${out ? ` (${out})` : ''}`)
  return 0
}

const [command, pkg, version, flag] = process.argv.slice(2)

if (!command || !pkg) {
  process.stderr.write('用法: npm-dist-tag.mjs resolve <package> <version> | repair <package> [version]\n')
  process.exit(2)
}

if (command === 'resolve') {
  if (!version) {
    process.stderr.write('resolve 需要 <version>\n')
    process.exit(2)
  }
  process.exit(resolveCmd(pkg, version))
} else if (command === 'repair') {
  const dryRun = version === '--dry-run' || flag === '--dry-run'
  const targetVersion = version === '--dry-run' ? '' : version
  process.exit(repairCmd(pkg, targetVersion, dryRun))
} else {
  process.stderr.write(`未知命令: ${command}\n`)
  process.exit(2)
}
