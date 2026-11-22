// Before `npm pack` or `npm publish`: downloads the three binaries from the GitHub release of this
// version (built by .github/workflows/release.yml), copies the license, and checks that the binary
// for this system reports the version in package.json.
"use strict";
const fs = require("node:fs");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const pkg = require("./package.json");
const assets = ["quietbrowser.exe", "quietbrowser-macos", "quietbrowser-linux-x64"];
const base = `https://github.com/emrehangorgec/quietbrowser/releases/download/v${pkg.version}`;

async function main() {
  for (const name of assets) {
    const res = await fetch(`${base}/${name}`);
    if (!res.ok) throw new Error(`${name}: HTTP ${res.status}. Push the v${pkg.version} tag and wait for the release workflow.`);
    const file = path.join(__dirname, name);
    fs.writeFileSync(file, Buffer.from(await res.arrayBuffer()));
    fs.chmodSync(file, 0o755);
    console.log(`${name}: ${fs.statSync(file).size} bytes`);
  }
  const own = { win32: "quietbrowser.exe", darwin: "quietbrowser-macos", linux: "quietbrowser-linux-x64" }[process.platform];
  if (own) {
    const version = execFileSync(path.join(__dirname, own), ["--version"], { encoding: "utf8" }).trim();
    if (version !== `quietbrowser ${pkg.version}`) throw new Error(`${own} says "${version}", package.json says ${pkg.version}`);
  }
  fs.copyFileSync(path.join(__dirname, "..", "LICENSE"), path.join(__dirname, "LICENSE"));
  console.log(`packed quietbrowser ${pkg.version}`);
}

main().catch((e) => {
  console.error(e.message);
  process.exit(1);
});
