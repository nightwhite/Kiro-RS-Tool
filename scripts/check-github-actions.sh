#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workflow="$script_dir/../.github/workflows/build.yml"

if [[ ! -f "$workflow" ]]; then
  echo "missing $workflow" >&2
  exit 1
fi

require_entry() {
  local entry="$1"

  if ! grep -Fq -- "$entry" "$workflow"; then
    echo "missing required workflow entry: $entry" >&2
    exit 1
  fi
}

require_entry "bun run build"
require_entry "cargo test --locked"
require_entry "cargo build --release --locked"
require_entry "actions/upload-artifact"
