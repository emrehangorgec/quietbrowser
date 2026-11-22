// Copies the release exe and the license into the package before `npm pack` or `npm publish`,
// and checks that the exe version matches package.json.
"use strict";
const fs = require("node:fs");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const root = path.join(__dirname, "..");
const exe = path.join(root, "target", "release", "quietbrowser.exe");
if (!fs.existsSync(exe)) throw new Error("build first: cargo build --release");
const version = execFileSync(exe, ["--version"], { encoding: "utf8" }).trim();
const pkg = require("./package.json");
if (version !== `quietbrowser ${pkg.version}`) throw new Error(`exe says "${version}", package.json says ${pkg.version}`);
fs.copyFileSync(exe, path.join(__dirname, "quietbrowser.exe"));
fs.copyFileSync(path.join(root, "LICENSE"), path.join(__dirname, "LICENSE"));
console.log(`packed ${version}`);
