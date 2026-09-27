#!/usr/bin/env node
// Runs the Baste binary from the platform package npm installed alongside
// this one (@weftsh/baste-<os>-<arch>).
"use strict";

const { spawnSync } = require("node:child_process");
const { chmodSync } = require("node:fs");
const path = require("node:path");

const packages = {
  "linux x64": "@weftsh/baste-linux-x64",
  "linux arm64": "@weftsh/baste-linux-arm64",
  "darwin arm64": "@weftsh/baste-darwin-arm64",
};
const docs = "https://github.com/Weftsh/baste#get-started";

function fail(message) {
  console.error(`baste: ${message}`);
  process.exit(1);
}

const platform = `${process.platform} ${process.arch}`;
const pkg = packages[platform];
if (!pkg) {
  if (process.platform === "win32") {
    fail(`on Windows, install and run Baste inside WSL2. See ${docs}`);
  }
  if (process.platform === "darwin") {
    fail(
      "Baste needs an Apple Silicon Mac. If this is one, this Node.js is an " +
        "x64 build running under Rosetta: install the arm64 Node.js, or use " +
        `the install script from ${docs}`,
    );
  }
  fail(`${process.platform}/${process.arch} is not supported. See ${docs}`);
}

let binary;
try {
  binary = path.join(path.dirname(require.resolve(`${pkg}/package.json`)), "bin", "baste");
} catch {
  fail(
    `${pkg} isn't installed. It's an optional dependency of @weftsh/baste, so ` +
      "reinstall without --no-optional or --omit=optional, or use the install " +
      `script from ${docs}`,
  );
}

function run() {
  return spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
}

let result = run();
if (result.error && result.error.code === "EACCES") {
  // Some package managers drop the executable bit when unpacking.
  try {
    chmodSync(binary, 0o755);
  } catch {}
  result = run();
}
if (result.error) {
  fail(`couldn't run ${binary}: ${result.error.message}`);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status === null ? 1 : result.status);
