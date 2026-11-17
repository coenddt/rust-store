#!/usr/bin/env node
// napi build 产出的 index.js 是平台分派 loader：21 个平台各自 `require('rust-store-node-<triple>')`
// 或 `require('./rust-store-node.<triple>.node')`，这些二进制/可选包在编译机与检查机上必然只存在其一，
// `tsc --checkJs` 会为每一条报 TS2307（模块静态不可解析），属生成产物的固有形态而非源码缺陷。
//
// 本脚本在 loader 首行补 `// @ts-nocheck`（只影响类型检查，不改变任何运行时行为），
// 挂在 build / build:debug 之后，保证每次重构生成都带上；已带则跳过（幂等）。

import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..')
const LOADER = join(ROOT, 'index.js')
const MARK = '// @ts-nocheck'

const src = readFileSync(LOADER, 'utf8')
if (src.split('\n').slice(0, 5).some((line) => line.trim() === MARK)) {
  console.log(`${LOADER}: 已有 ${MARK}，跳过`)
  process.exit(0)
}

// 插在首行之前：@ts-nocheck 需位于文件顶部才能关闭整文件检查。
writeFileSync(LOADER, `${MARK}\n${src}`)
console.log(`${LOADER}: 已插入 ${MARK}`)
