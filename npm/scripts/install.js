#!/usr/bin/env node

const https = require("https");
const fs = require("fs");
const path = require("path");
const { execSync } = require("child_process");
const zlib = require("zlib");
const assert = require("assert");

const BIN_DIR = path.join(__dirname, "..", "bin");
const BINARY_NAME = process.platform === "win32" ? "debugger-mcp.exe" : "debugger-mcp";
const BINARY_PATH = path.join(BIN_DIR, BINARY_NAME);

const PLATFORM_MAP = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "linux-x64": "x86_64-unknown-linux-gnu",
};

function getTarget() {
  const key = `${process.platform}-${process.arch}`;
  const target = PLATFORM_MAP[key];
  if (!target) {
    throw new Error(
      `Unsupported platform: ${key}. Supported: ${Object.keys(PLATFORM_MAP).join(", ")}`
    );
  }
  return target;
}

function getPackageJson() {
  return JSON.parse(
    fs.readFileSync(path.join(__dirname, "..", "package.json"), "utf8")
  );
}

function getRepo() {
  const pkg = getPackageJson();
  const url = typeof pkg.repository === "string" ? pkg.repository : pkg.repository.url;
  const match = url.match(/github\.com\/([^/]+\/[^/.]+)/);
  assert(match, `Cannot extract GitHub repo from repository URL: ${url}`);
  return match[1];
}

function fetch(url) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "User-Agent": "debugger-mcp-npm" } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          return fetch(res.headers.location).then(resolve, reject);
        }
        if (res.statusCode !== 200) {
          return reject(new Error(`HTTP ${res.statusCode} for ${url}`));
        }
        const chunks = [];
        res.on("data", (chunk) => chunks.push(chunk));
        res.on("end", () => resolve(Buffer.concat(chunks)));
        res.on("error", reject);
      })
      .on("error", reject);
  });
}

async function install() {
  const target = getTarget();
  const pkg = getPackageJson();
  const repo = getRepo();
  const version = pkg.version;
  const assetName = `debugger-mcp-${target}.tar.gz`;
  const url = `https://github.com/${repo}/releases/download/v${version}/${assetName}`;

  console.log(`Downloading debugger-mcp v${version} for ${target}...`);

  const data = await fetch(url);

  const tarPath = path.join(BIN_DIR, "tmp.tar.gz");
  fs.mkdirSync(BIN_DIR, { recursive: true });
  fs.writeFileSync(tarPath, data);

  // Extract — the archive contains a single file: debugger_mcp
  execSync(`tar xzf "${tarPath}" -C "${BIN_DIR}"`, { stdio: "inherit" });
  fs.unlinkSync(tarPath);

  // The Rust binary is named debugger_mcp (underscore), rename to debugger-mcp (hyphen)
  const extractedPath = path.join(BIN_DIR, "debugger_mcp");
  if (fs.existsSync(extractedPath)) {
    fs.renameSync(extractedPath, BINARY_PATH);
  }

  fs.chmodSync(BINARY_PATH, 0o755);
  console.log(`Installed debugger-mcp to ${BINARY_PATH}`);
}

install().catch((err) => {
  console.error(`Failed to install debugger-mcp: ${err.message}`);
  console.error("You can install manually: cargo install --git " + getPackageJson().repository.url);
  process.exit(1);
});
