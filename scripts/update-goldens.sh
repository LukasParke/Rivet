#!/usr/bin/env bash
# Regenerate all committed golden artifacts:
#   - packages/contracts/openapi.charte.json (+ facts) via charte-rs
#   - tests/golden/parser/* via GOLDEN_UPDATE=1 parser_golden
#   - tests/golden/api/routes.json via GOLDEN_UPDATE=1 api_golden
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$SCRIPT_DIR/.."

~/github/charte.rs/target/release/charte-rs generate "$ROOT_DIR" \
  --output packages/contracts/openapi.charte.json \
  --facts-output packages/contracts/charte.facts.json \
  --diagnostics-output /dev/null

GOLDEN_UPDATE=1 cargo test --test parser_golden
GOLDEN_UPDATE=1 cargo test --test api_golden

echo done.
