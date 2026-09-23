// Fetches the platform binary at install time so the first `dejavu` call is instant. Failures only
// warn: the launcher retries on first use. Bun skips this hook for untrusted packages.
"use strict";

const fs = require("node:fs");
const path = require("node:path");
const { ensureBinary, packageRoot } = require("./npm-binary.cjs");

const log = (message) => process.stderr.write(`${message}\n`);

// `bun install` in a source checkout runs this hook too; the checkout runs from source.
if (!fs.existsSync(path.join(packageRoot, ".git"))) ensureBinary({ log })
  .then((binary) => log(`dejavu: installed binary at ${binary}`))
  .catch((error) => {
    log(`dejavu: ${error instanceof Error ? error.message : String(error)}`);
    log("dejavu: the binary will be fetched again the first time you run `dejavu`.");
  });
