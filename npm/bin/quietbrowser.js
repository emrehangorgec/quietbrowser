#!/usr/bin/env node
// Runs the bundled quietbrowser.exe with the same arguments, stdio and exit code.
"use strict";
const { spawnSync } = require("node:child_process");
const path = require("node:path");

if (process.platform !== "win32") {
  console.error("quietbrowser runs on Windows only (it uses Microsoft Edge WebView2).");
  process.exit(1);
}

const exe = path.join(__dirname, "..", "quietbrowser.exe");
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
