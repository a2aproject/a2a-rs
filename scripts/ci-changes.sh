#!/usr/bin/env bash
# Decides whether a change needs the Rust jobs. Writes `code=true` or
# `code=false` to $GITHUB_OUTPUT.
#
# Only a pull request or a merge queue entry that touches nothing but the docs
# site yields `false`. Every other event, and any doubt, yields `true`.
#
# Keep the list below in step with the `paths-ignore` of fuzz.yml and
# release-binaries.yml.
set -euo pipefail

docs_only='^(docs/|mkdocs\.yml$|requirements-docs\.(in|txt)$|lychee\.toml$|\.typos\.toml$|\.github/workflows/docs\.yml$)'

emit() {
    echo "code=$1" >> "${GITHUB_OUTPUT:-/dev/stdout}"
    echo "code=$1: $2"
}

case "${GITHUB_EVENT_NAME:-}" in
    pull_request | merge_group) ;;
    *)
        emit true "a ${GITHUB_EVENT_NAME:-unknown} event always runs everything"
        exit 0
        ;;
esac

# HEAD is the merge commit of the pull request, or the merge queue entry. Its
# first parent is the base, so the diff is this change and nothing else.
if ! files=$(git diff --name-only --no-renames HEAD^1 HEAD 2>/dev/null) || [ -z "$files" ]; then
    emit true "could not list the changed files"
    exit 0
fi

if printf '%s\n' "$files" | grep -qvE "$docs_only"; then
    emit true "the change touches more than the docs site"
else
    emit false "the change touches only the docs site"
fi
