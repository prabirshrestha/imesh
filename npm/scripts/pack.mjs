#!/usr/bin/env node
// Makes the npm packages from the bundle that `cargo x build` writes to dist/.
//
// Usage: node npm/scripts/pack.mjs <version> <dist-dir> <out-dir>
//
// <out-dir> gets one directory per package, ready for `npm publish`.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const [version, distDir, outDir] = process.argv.slice(2);
if (!version || !distDir || !outDir) {
  console.error("usage: pack.mjs <version> <dist-dir> <out-dir>");
  process.exit(2);
}

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.join(here, "..", "..");
const mainSrc = path.join(here, "..", "iroh-mesh");
const main = JSON.parse(fs.readFileSync(path.join(mainSrc, "package.json"), "utf8"));

const triples = {
  "darwin-arm64": "aarch64-apple-darwin",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "linux-x64": "x86_64-unknown-linux-musl",
  "windows-x64": "x86_64-pc-windows-gnu",
};

const common = { version, license: main.license, repository: main.repository };
const write = (dir, pkg, readme) => {
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
  fs.writeFileSync(path.join(dir, "README.md"), readme);
};

for (const name of Object.keys(main.optionalDependencies)) {
  const platform = name.slice("iroh-mesh-".length);
  const [name_os, cpu] = platform.split("-");
  // The package name says "windows"; npm's os field needs "win32".
  const os = name_os === "windows" ? "win32" : name_os;
  const triple = triples[platform];
  if (!triple) throw new Error(`no target triple for ${name}`);
  const exe = os === "win32" ? "imesh.exe" : "imesh";
  const src = path.join(distDir, "imesh", "targets", triple, exe);
  if (!fs.existsSync(src)) throw new Error(`missing binary: ${src}`);
  const dir = path.join(outDir, name);
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
  fs.copyFileSync(src, path.join(dir, "bin", exe));
  fs.chmodSync(path.join(dir, "bin", exe), 0o755);
  write(
    dir,
    { name, ...common, description: `The imesh binary for ${platform}`, os: [os], cpu: [cpu], files: ["bin"], preferUnplugged: true },
    `# ${name}\n\nThe \`imesh\` binary for ${platform}. Install \`iroh-mesh\` instead.\n`
  );
}

const bundleDir = path.join(outDir, "iroh-mesh-bundle");
fs.mkdirSync(bundleDir, { recursive: true });
fs.copyFileSync(path.join(distDir, "imesh.zip"), path.join(bundleDir, "imesh.zip"));
write(
  bundleDir,
  { name: "iroh-mesh-bundle", ...common, description: "The imesh binaries for every platform, for remote installs", files: ["imesh.zip"] },
  "# iroh-mesh-bundle\n\nThe imesh binaries for every platform, for `imesh add` and `imesh update`. Install `iroh-mesh` instead.\n"
);

const mainOut = path.join(outDir, "iroh-mesh");
fs.cpSync(mainSrc, mainOut, { recursive: true });
main.version = version;
main.dependencies["iroh-mesh-bundle"] = version;
for (const k of Object.keys(main.optionalDependencies)) main.optionalDependencies[k] = version;
fs.writeFileSync(path.join(mainOut, "package.json"), JSON.stringify(main, null, 2) + "\n");
fs.copyFileSync(path.join(root, "README.md"), path.join(mainOut, "README.md"));
fs.chmodSync(path.join(mainOut, "bin", "imesh.js"), 0o755);

console.log(`packages in ${outDir}: ${fs.readdirSync(outDir).join(", ")}`);
