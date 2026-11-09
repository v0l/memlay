#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/dashboard/dist/index.html"

cd "$root/dashboard"
bun install --frozen-lockfile
bun run build

printf 'wrote %s (%s)\n' "$out" "$(du -h "$out" | cut -f1)"
