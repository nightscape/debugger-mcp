#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$ROOT_DIR/bin"

echo "Building debugger-mcp..."
cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml"

mkdir -p "$BIN_DIR"
cp "$ROOT_DIR/target/release/debugger_mcp" "$BIN_DIR/debugger-mcp"

echo "Installed to $BIN_DIR/debugger-mcp"
