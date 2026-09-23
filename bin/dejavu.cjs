#!/usr/bin/env node
// npm launcher: runs the native dejavu binary for this platform. A source checkout, or a platform
// without a verified binary, runs src/cli.ts with Bun instead.
"use strict";

const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
const { ensureBinary, packageRoot } = require("../scripts/npm-binary.cjs");

const log = (message) => process.stderr.write(`${message}\n`);

function run(command, args) {
  const result = spawnSync(command, args, { stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.signal) process.kill(process.pid, result.signal);
  else process.exit(result.status ?? 1);
}

function runSource() {
  run("bun", [path.join(packageRoot, "src", "cli.ts"), ...process.argv.slice(2)]);
}

if (fs.existsSync(path.join(packageRoot, ".git"))) {
  runSource();
} else {
  ensureBinary({ log })
    .then((binary) => run(binary, process.argv.slice(2)))
    .catch((error) => {
      log(`dejavu: ${error instanceof Error ? error.message : String(error)}`);
      log("dejavu: falling back to the bundled source, which needs Bun 1.4.2 or newer (https://bun.sh)");
      try {
        runSource();
      } catch (fallback) {
        log(`dejavu: ${fallback instanceof Error ? fallback.message : String(fallback)}`);
        process.exit(1);
      }
    });
}
