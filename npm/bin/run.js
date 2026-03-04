#!/usr/bin/env node

const { spawnSync } = require("child_process");
const path = require("path");
const fs = require("fs");

const BINARY_NAME = process.platform === "win32" ? "debugger-mcp.exe" : "debugger-mcp";
const BINARY_PATH = path.join(__dirname, BINARY_NAME);

if (!fs.existsSync(BINARY_PATH)) {
  console.error("debugger-mcp binary not found. Running install...");
  const installResult = spawnSync(
    process.execPath,
    [path.join(__dirname, "..", "scripts", "install.js")],
    { stdio: "inherit" }
  );
  if (installResult.status !== 0) {
    process.exit(1);
  }
}

const result = spawnSync(BINARY_PATH, process.argv.slice(2), { stdio: "inherit" });
process.exit(result.status ?? 1);
