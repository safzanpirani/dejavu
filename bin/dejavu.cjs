#!/usr/bin/env node
// npm launcher: runs the native dejavu binary for this platform. A source checkout runs its own
// release build from target/release instead.
"use strict";

const { spawnSync } = require("node:child_process");
const { ensureBinary, isSourceCheckout, sourceBinary } = require("../scripts/npm-binary.cjs");

const log = (message) => process.stderr.write(`${message}\n`);

function run(command, args) {
  const result = spawnSync(command, args, { stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.signal) process.kill(process.pid, result.signal);
  else process.exit(result.status ?? 1);
}

function fail(error) {
  log(`dejavu: ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
}

if (isSourceCheckout()) {
  const binary = sourceBinary();
  if (!binary) {
    log("dejavu: this source checkout has no release build yet; build it with");
    log("  cargo build --release -p dejavu");
    log("or install it on your PATH with scripts/install-local.sh");
    process.exit(1);
  }
  try {
    run(binary, process.argv.slice(2));
  } catch (error) {
    fail(error);
  }
} else {
  ensureBinary({ log })
    .then((binary) => run(binary, process.argv.slice(2)))
    .catch((error) => {
      log(`dejavu: ${error instanceof Error ? error.message : String(error)}`);
      log("dejavu: download a release binary from https://github.com/safzanpirani/dejavu/releases, or build from source with `cargo build --release -p dejavu`");
      process.exit(1);
    });
}
