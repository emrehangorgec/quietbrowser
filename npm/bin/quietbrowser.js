#!/usr/bin/env node
// Runs the bundled quietbrowser binary for this system with the same arguments, stdio and exit code.
"use strict";
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const binaries = {
  win32: "quietbrowser.exe", // x64, also runs on Windows on Arm
  darwin: "quietbrowser-macos", // Intel and Apple Silicon
  linux: process.arch === "x64" ? "quietbrowser-linux-x64" : null,
};
const name = binaries[process.platform];
if (!name) {
  console.error(`quietbrowser has no prebuilt binary for ${process.platform} ${process.arch}. Build it from source: https://github.com/emrehangorgec/quietbrowser`);
  process.exit(1);
}

const exe = path.join(__dirname, "..", name);
if (process.platform !== "win32") {
  // a package packed on Windows loses the executable bit
  try {
    fs.chmodSync(exe, 0o755);
  } catch {}
}
const env = { ...process.env };
// Tells `install-skill` how the agent should run quietbrowser later.
if (!env.QUIETBROWSER_LAUNCHER) {
  env.QUIETBROWSER_LAUNCHER = __dirname.includes(`${path.sep}_npx${path.sep}`) ? "npx -y quietbrowser" : "quietbrowser";
}
const r = spawnSync(exe, process.argv.slice(2), { stdio: "inherit", env, windowsHide: true });
if (r.error) {
  console.error(`error: could not run ${exe}: ${r.error.message}`);
  process.exit(2);
}
process.exit(r.status === null ? 1 : r.status);
