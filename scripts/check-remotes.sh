#!/usr/bin/env bash
# 校验 origin(GitHub) 与 gitee 的 main 与全部 tag 是否完全一致。
#
# 为什么需要：GitHub Actions 用的是 **tag 所指 commit 里**的 workflow，不是 main 上的
# 最新版本。一条只推到单端的修复，会从另一端推 tag 时被绕过（2026-11-13 实测：npm
# latest 降级修复只推到 GitHub，从 gitee 血统推 tag 会复现该 bug）。所以 main 与 tag
# 必须在两端同时落地，且能被机械核验，而不是靠记忆。
#
# 用法：
#   bash scripts/check-remotes.sh            # 核对 origin 与 gitee
#   bash scripts/check-remotes.sh upstream fork
#
# GitHub 侧优先走 git ls-remote；国内网络下 git 传输偶发 "Connection was reset"，
# 此时自动回退到 GitHub API（gh）。回退需要 gh 已认证：export GH_TOKEN=<token>。
#
# 退出码：0 = 完全一致；1 = 存在差异；2 = 远端读取失败（网络/认证，无法判定）。

set -uo pipefail

ORIGIN=${1:-origin}
GITEE=${2:-gitee}
RETRIES=5

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

# 取远端引用，统一输出成 git ls-remote 的格式：<sha>\t<ref>
refs_of_remote() {
  local remote=$1 out=$2 url slug i
  for i in $(seq 1 "$RETRIES"); do
    if git ls-remote --heads --tags "$remote" >"$out" 2>/dev/null && [ -s "$out" ]; then
      return 0
    fi
    sleep 3
  done
  # 回退：只对 GitHub 有 API 可用；其它远端（如 gitee）只能等网络恢复。
  url=$(git remote get-url "$remote" 2>/dev/null) || return 1
  slug=$(printf '%s' "$url" | sed -n 's#.*github\.com[:/]\([^/]*/[^/]*\)\.git$#\1#p')
  [ -n "$slug" ] || return 1
  command -v gh >/dev/null 2>&1 || return 1
  {
    gh api "repos/$slug/git/ref/heads/main" -q '.object.sha + "\t" + .ref' 2>/dev/null
    gh api --paginate "repos/$slug/git/refs/tags" -q '.[] | .object.sha + "\t" + .ref' 2>/dev/null
  } >"$out"
  [ -s "$out" ]
}

die_unreadable() {
  echo "无法读取远端 $1 的引用（git 传输与 API 回退均失败）" >&2
  echo "→ 这不能判定为「不一致」；GitHub 侧若 keyring token 失效，请 export GH_TOKEN=<token> 后重跑" >&2
  exit 2
}

main_of() {
  awk '$2 == "refs/heads/main" { print $1; exit }' "$1"
}

# tag 自身 sha：annotated tag 是 tag object 的 sha，轻量 tag 是 commit sha。
# 两端同类 tag 必然同 sha；类型不同时保守判为差异，交人工确认。
tags_of() {
  awk '
    $2 ~ /^refs\/tags\// && $2 !~ /\^\{\}$/ {
      t = $2; sub(/^refs\/tags\//, "", t); print t "\t" $1
    }
  ' "$1" | sort
}

refs_of_remote "$ORIGIN" "$tmpdir/o.refs" || die_unreadable "$ORIGIN"
refs_of_remote "$GITEE" "$tmpdir/g.refs" || die_unreadable "$GITEE"

tags_of "$tmpdir/o.refs" >"$tmpdir/o.tags"
tags_of "$tmpdir/g.refs" >"$tmpdir/g.tags"

status=0

om=$(main_of "$tmpdir/o.refs")
gm=$(main_of "$tmpdir/g.refs")
if [ -z "$om" ] || [ -z "$gm" ]; then
  echo "main: 至少一端没有 main 分支（$ORIGIN='$om' $GITEE='$gm'）"
  status=1
elif [ "$om" = "$gm" ]; then
  echo "main ✅ ${om:0:9}（两端一致）"
else
  echo "main ❌ $ORIGIN=${om:0:9} $GITEE=${gm:0:9}"
  status=1
fi

on_count=$(wc -l <"$tmpdir/o.tags" | tr -d ' ')
gn_count=$(wc -l <"$tmpdir/g.tags" | tr -d ' ')

join -t$'\t' "$tmpdir/o.tags" "$tmpdir/g.tags" 2>/dev/null | sort >"$tmpdir/joined"
mismatched=$(awk -F'\t' '$2 != $3 { print $1 }' "$tmpdir/joined")
only_o=$(comm -23 <(cut -f1 "$tmpdir/o.tags") <(cut -f1 "$tmpdir/g.tags"))
only_g=$(comm -13 <(cut -f1 "$tmpdir/o.tags") <(cut -f1 "$tmpdir/g.tags"))

if [ -z "$mismatched" ] && [ -z "$only_o" ] && [ -z "$only_g" ]; then
  echo "tags ✅ ${on_count} 个（名称与指向全部一致）"
else
  status=1
  echo "tags ❌ $ORIGIN=${on_count} $GITEE=${gn_count}"
  [ -n "$mismatched" ] && echo "$mismatched" | sed 's/^/  指向不同: /'
  [ -n "$only_o" ] && echo "$only_o" | sed "s/^/  仅 $ORIGIN 有: /"
  [ -n "$only_g" ] && echo "$only_g" | sed "s/^/  仅 $GITEE 有: /"
fi

if [ "$status" -eq 0 ]; then
  echo "→ 两端一致"
else
  echo "→ 存在差异，用 git push <remote> main / git push -f <remote> refs/tags/<tag> 对齐" >&2
fi
exit "$status"
