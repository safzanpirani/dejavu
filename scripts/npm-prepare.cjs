// Release helper: pins the published package to the binaries attached to its GitHub release.
//   node scripts/npm-prepare.cjs <checksums.txt>
"use strict";

const fs = require("node:fs");
const path = require("node:path");

const [checksumFile] = process.argv.slice(2);
if (!checksumFile) {
  console.error("usage: npm-prepare.cjs <checksums.txt>");
  process.exit(2);
}
const checksums = {};
for (const line of fs.readFileSync(checksumFile, "utf8").split("\n")) {
  const match = /^([0-9a-f]{64})\s+\*?(\S+)$/.exec(line.trim());
  if (match) checksums[path.basename(match[2])] = match[1];
}
if (Object.keys(checksums).length === 0) {
  console.error(`no checksums found in ${checksumFile}`);
  process.exit(1);
}
const root = path.resolve(__dirname, "..");
fs.writeFileSync(path.join(root, "checksums.json"), `${JSON.stringify(checksums, null, 2)}\n`);
const { name, version } = require(path.join(root, "package.json"));
console.log(`prepared ${name}@${version} with ${Object.keys(checksums).length} pinned binaries`);
