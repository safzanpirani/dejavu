// Fetches the platform binary at install time so the first `dejavu` call is instant. Failures only
// warn: the launcher retries on first use. Bun skips this hook for untrusted packages.
"use strict";

const { ensureBinary, isSourceCheckout } = require("./npm-binary.cjs");

const log = (message) => process.stderr.write(`${message}\n`);

// `bun install` in a source checkout runs this hook too; the checkout runs its own cargo build.
if (!isSourceCheckout()) ensureBinary({ log })
  .then((binary) => log(`dejavu: installed binary at ${binary}`))
  .catch((error) => {
    log(`dejavu: ${error instanceof Error ? error.message : String(error)}`);
    log("dejavu: the binary will be fetched again the first time you run `dejavu`.");
  });
