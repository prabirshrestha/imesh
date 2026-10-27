#!/usr/bin/env node
// Runs the imesh binary from the platform package that npm installed.
"use strict";

const { spawnSync } = require("child_process");

const pkg = `imesh-${process.platform}-${process.arch}`;
const exe = process.platform === "win32" ? "imesh.exe" : "imesh";

let bin;
try {
  bin = require.resolve(`${pkg}/bin/${exe}`);
} catch {
  console.error(
    `imesh: no binary for ${process.platform}-${process.arch}.\n` +
      `The package "${pkg}" is not installed. Make sure that optional dependencies are not disabled.`
  );
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`imesh: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
