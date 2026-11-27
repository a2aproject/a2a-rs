#!/usr/bin/env bash
# Publishes an export made by scripts/export_docs.py as the one commit of the
# `docs-export` branch, which the A2A site mounts. The branch is replaced on
# every run and is never edited by hand.
#
# Usage: scripts/publish_docs_export.sh EXPORT_DIR
# Needs GITHUB_TOKEN and GITHUB_REPOSITORY (as in Actions), or DOCS_EXPORT_REMOTE.
set -euo pipefail

export_dir=${1:?usage: publish_docs_export.sh EXPORT_DIR}
remote=${DOCS_EXPORT_REMOTE:-https://github.com/${GITHUB_REPOSITORY:-}.git}
source_sha=${GITHUB_SHA:-a local checkout}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The mount reads `docs/` of the branch: it rejects `.` as a source directory.
mkdir "$work/docs"
cp -R "$export_dir/." "$work/docs/"
printf 'Generated from %s by scripts/export_docs.py. Do not edit.\n' "$source_sha" >"$work/README.md"

git -C "$work" init -q -b docs-export
git -C "$work" add -A
git -C "$work" -c commit.gpgsign=false \
  -c user.name='github-actions[bot]' \
  -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
  commit -q -m "docs: export of $source_sha"

auth=()
if [ -n "${GITHUB_TOKEN:-}" ]; then
  basic=$(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 | tr -d '\n')
  auth=(-c "http.https://github.com/.extraheader=Authorization: Basic $basic")
fi
git -C "$work" ${auth[@]+"${auth[@]}"} push -q --force "$remote" docs-export
