#!/usr/bin/env node
// Runs the imesh binary from the platform package that npm installed. Remote installs
// (`imesh add`, `imesh update server`) use the bundle from the iroh-mesh-bundle package.
"use strict";

const { spawnSync } = require("child_process");

// npm refused the name iroh-mesh-win32-x64, so the Windows package uses "windows".
const os = process.platform === "win32" ? "windows" : process.platform;
const pkg = `iroh-mesh-${os}-${process.arch}`;
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

// Installs on other machines need the binaries for every platform, and `imesh update` installs
// from a bundle. Other commands, such as `imesh up`, install this binary and need no bundle.
const args = process.argv.slice(2);
// Skip global options and their values: `imesh -c work add ...`.
const withValue = new Set(["-c", "--context", "--log-file"]);
let command;
for (let i = 0; i < args.length; i++) {
  if (withValue.has(args[i])) { i++; continue; }
  if (!args[i].startsWith("-")) { command = args[i]; break; }
}
const remote = command === "add" || command === "update";
const env = { ...process.env };
if (remote && !env.IMESH_BUNDLE) {
  try {
    env.IMESH_BUNDLE = require.resolve("iroh-mesh-bundle/imesh.zip");
  } catch {}
}

// The child gets Ctrl+C too. Wait for its exit code instead of exiting first.
const ignore = () => {};
process.on("SIGINT", ignore);
const result = spawnSync(bin, args, { stdio: "inherit", env });
if (result.error) {
  console.error(`imesh: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  process.off("SIGINT", ignore);
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
