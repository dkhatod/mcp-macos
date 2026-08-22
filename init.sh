#!/bin/sh
# One-shot verification for mcp-macos. Exit 0 = everything green.
set -e
cd "$(dirname "$0")"

echo "== cargo fmt --check"
cargo fmt --check

echo "== cargo clippy -- -D warnings"
cargo clippy -- -D warnings

echo "== cargo test"
cargo test

echo "== stdio smoke: initialize + tools/list"
SMOKE=$(mktemp -d)
trap 'rm -rf "$SMOKE"' EXIT
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | cargo run -q -- --state-dir "$SMOKE" > "$SMOKE/out.ndjson"

grep -q '"serverInfo"' "$SMOKE/out.ndjson" || { echo "FAIL: no initialize response"; exit 1; }
grep -q '"permissions_check"' "$SMOKE/out.ndjson" || { echo "FAIL: tools/list missing"; exit 1; }

echo "ALL GREEN"
