#!/usr/bin/env node
// Makes the npm packages from prebuilt binaries.
//
// Usage: node npm/scripts/pack.mjs <version> <dist-dir> <out-dir>
//
// <dist-dir> has one directory per platform, for example
// dist/linux-x64/imesh and dist/win32-x64/imesh.exe.
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
const mainSrc = path.join(here, "..", "imesh");
const main = JSON.parse(fs.readFileSync(path.join(mainSrc, "package.json"), "utf8"));

const platforms = Object.keys(main.optionalDependencies).map((name) => {
  const [, os, cpu] = name.split("-");
  return { name, os, cpu };
});

fs.mkdirSync(outDir, { recursive: true });

for (const { name, os, cpu } of platforms) {
  const exe = os === "win32" ? "imesh.exe" : "imesh";
  const src = path.join(distDir, `${os}-${cpu}`, exe);
  if (!fs.existsSync(src)) {
    console.error(`missing binary: ${src}`);
    process.exit(1);
  }
  const dir = path.join(outDir, name);
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
  fs.copyFileSync(src, path.join(dir, "bin", exe));
  fs.chmodSync(path.join(dir, "bin", exe), 0o755);
  const pkg = {
    name,
    version,
    description: `The imesh binary for ${os}-${cpu}`,
    repository: main.repository,
    os: [os],
    cpu: [cpu],
    files: ["bin"],
    preferUnplugged: true,
  };
  fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
  fs.writeFileSync(
    path.join(dir, "README.md"),
    `# ${name}\n\nThe \`imesh\` binary for ${os}-${cpu}. Install \`imesh\` instead.\n`
  );
}

const mainOut = path.join(outDir, "imesh");
fs.cpSync(mainSrc, mainOut, { recursive: true });
main.version = version;
for (const k of Object.keys(main.optionalDependencies)) main.optionalDependencies[k] = version;
fs.writeFileSync(path.join(mainOut, "package.json"), JSON.stringify(main, null, 2) + "\n");
fs.chmodSync(path.join(mainOut, "bin", "imesh.js"), 0o755);

console.log(`packages in ${outDir}: ${[...platforms.map((p) => p.name), "imesh"].join(", ")}`);
