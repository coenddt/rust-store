#!/usr/bin/env bash
#
# Assemble the GitHub Pages source tree for this repository.
#
# The walkthroughs in doc/use-cases/ and the root README.md are the single source of truth:
# they are copied in at build time and never duplicated inside the repository. Everything
# that has to differ between the repository and the published site is handled here:
#
#   * page-relative Markdown links are rewritten to the .html URLs Jekyll emits;
#   * links to files that are not part of the site (LICENSE, README.zh-CN.md, .trae/...)
#     are pointed at GitHub instead;
#   * every page body is wrapped in {% raw %} because Liquid runs before the Markdown
#     converter and would otherwise swallow the literal {{...}} placeholders that appear
#     in the engine documentation;
#   * front matter (title / description) is derived from the first heading and the first
#     prose paragraph, so each page gets a unique <title> and meta description.
#
# Required environment:
#   REPO_SLUG  owner/repo, e.g. coenddt/nodejs-store
#   BASEURL    site origin + baseurl without a trailing slash,
#              e.g. https://coenddt.github.io/nodejs-store
# Optional environment:
#   BRANCH     branch used for GitHub blob links (default: main)
set -euo pipefail

REPO_SLUG="${REPO_SLUG:?REPO_SLUG is required}"
BASEURL="${BASEURL:?BASEURL is required}"
BRANCH="${BRANCH:-main}"

SRC="_site_src"

rm -rf "$SRC"
mkdir -p "$SRC/use-cases"

cp .github/pages/_config.yml "$SRC/_config.yml"
cp -r .github/pages/_layouts "$SRC/_layouts"
cp .github/pages/index.md "$SRC/index.md"
cp .github/pages/robots.txt "$SRC/robots.txt"

cp README.md "$SRC/readme.md"
cp llms.txt "$SRC/llms.txt"

cp doc/use-cases/*.md "$SRC/use-cases/"
mv "$SRC/use-cases/README.md" "$SRC/use-cases/index.md"

# --- link rewrites -------------------------------------------------------------------
# Walkthrough pages and the walkthrough index.
find "$SRC/use-cases" -name '*.md' -print0 | xargs -0 sed -i \
  -e 's#](\.\./\.\./README\.md#](../readme.html#g' \
  -e "s#](\.\./\.\./\.trae/#](https://github.com/${REPO_SLUG}/blob/${BRANCH}/.trae/#g" \
  -e 's#](\([0-9][0-9]-[a-z0-9-]*\)\.md)#](\1.html)#g'

# The README page: files that are not part of the site live on GitHub.
sed -i \
  -e "s#](README\.zh-CN\.md)#](https://github.com/${REPO_SLUG}/blob/${BRANCH}/README.zh-CN.md)#g" \
  -e "s#](LICENSE)#](https://github.com/${REPO_SLUG}/blob/${BRANCH}/LICENSE)#g" \
  -e 's#](doc/use-cases/)#](use-cases/)#g' \
  "$SRC/readme.md"

# --- front matter + Liquid escaping ---------------------------------------------------
# Title comes from the first heading (or an explicit override); the description is the
# first prose paragraph long enough to be useful. Markdown emphasis is stripped so the
# values read as plain text in a <meta> tag, and an empty description is omitted
# altogether so the layout falls back to the site-wide one.
inject_front_matter() {
  local file="$1" override="${2:-}" title description
  title="${override:-$(sed -n 's/^# \{1,\}\(.*\)$/\1/p' "$file" | head -n 1)}"
  description="$(awk 'length($0) >= 50 && $0 !~ /^[[:space:]]/ && $0 !~ /^[#>|!]/ && $0 !~ /^```/ { print; exit }' "$file")"
  title="$(printf '%s' "$title" | tr -d '"' | sed -E 's/[*`]//g' | cut -c1-120)"
  description="$(printf '%s' "$description" | tr -d '"' | sed -E 's/[*`]//g' | cut -c1-180)"
  {
    printf -- '---\n'
    printf 'title: "%s"\n' "$title"
    if [ -n "$description" ]; then
      printf 'description: "%s"\n' "$description"
    fi
    printf -- '---\n'
    printf '\n'
    printf '{%% raw %%}\n'
    cat "$file"
    printf '\n{%% endraw %%}\n'
  } > "${file}.tmp"
  mv "${file}.tmp" "$file"
}

inject_front_matter "$SRC/readme.md" "Documentation"
for f in "$SRC"/use-cases/*.md; do
  inject_front_matter "$f"
done

# --- sitemap --------------------------------------------------------------------------
{
  printf '<?xml version="1.0" encoding="UTF-8"?>\n'
  printf '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n'
  while IFS= read -r path; do
    case "$path" in
      index.md) url="${BASEURL}/" ;;
      */index.md) url="${BASEURL}/${path%index.md}" ;;
      *) url="${BASEURL}/${path%.md}.html" ;;
    esac
    printf '  <url><loc>%s</loc></url>\n' "$url"
  done < <(cd "$SRC" && find . -name '*.md' -printf '%P\n' | sort)
  printf '</urlset>\n'
} > "$SRC/sitemap.xml"

# --- config availability --------------------------------------------------------------
# Jekyll resolves its default _config.yml relative to the process working directory, which
# for this action is the workspace root, while --source points at $SRC. Keeping an
# identical copy in both places makes the build independent of which one it picks up.
cp .github/pages/_config.yml ./_config.yml

echo "--- assembled site source ---"
(cd "$SRC" && find . -type f | sort)
echo "--- sitemap ---"
cat "$SRC/sitemap.xml"
