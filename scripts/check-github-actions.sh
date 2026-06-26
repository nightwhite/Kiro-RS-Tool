#!/usr/bin/env bash
set -euo pipefail

workflow=".github/workflows/build.yml"

if [[ ! -f "$workflow" ]]; then
  echo "missing $workflow" >&2
  exit 1
fi

grep -q "bun run build" "$workflow"
grep -q "cargo test --locked" "$workflow"
grep -q "cargo build --release --locked" "$workflow"
grep -q "actions/upload-artifact" "$workflow"
