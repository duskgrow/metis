import { execFileSync } from "node:child_process";
import { chmod, copyFile, mkdir, readdir, readFile, realpath, rm, stat, writeFile } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { nativeFiles } from "./native-files.mjs";
import { pruneProductionPackages } from "./prune-production.mjs";

const workspace = fileURLToPath(new URL("..", import.meta.url));
const executableSuffix = process.platform === "win32" ? ".exe" : "";
const cargo = process.platform === "win32" ? "cargo.exe" : "cargo";
const pnpm = process.platform === "win32" ? "pnpm.cmd" : "pnpm";
const rustc = process.platform === "win32" ? "rustc.exe" : "rustc";

function run(command, args, options = {}) {
  execFileSync(command, args, { cwd: workspace, stdio: "inherit", ...options });
}

function capture(command, args, options = {}) {
  return execFileSync(command, args, {
    cwd: workspace,
    encoding: "utf8",
    timeout: 30_000,
    ...options,
  });
}

function contained(root, candidate) {
  const path = relative(root, candidate);
  return path === "" || (!isAbsolute(path) && path !== ".." && !path.startsWith(`..${sep}`));
}

async function collectNativeDependencies(
  binary,
  packageDirectory,
  libraryPaths,
  loaderPath,
) {
  const libraryPath = [...new Set([...libraryPaths].map((path) => dirname(path)))].join(":");
  const dependencies = loaderPath
    ? capture(loaderPath, ["--list", "--library-path", libraryPath, binary])
    : capture("ldd", [binary]);
  if (dependencies.includes("=> not found")) {
    throw new Error(`unresolved native dependency for ${binary}:\n${dependencies}`);
  }
  let reportedLoader;
  for (const line of dependencies.split("\n")) {
    const resolved = line.match(/=>\s*(\/[^\s]+)/u)?.[1];
    const loader = line.match(/^\s*(\/[^\s]*ld-[^\s]*)\s+\(0x/u)?.[1];
    for (const path of [resolved, loader]) {
      if (!path || contained(packageDirectory, await realpath(path))) continue;
      if (basename(path).startsWith("ld-")) reportedLoader = path;
      libraryPaths.add(path);
    }
  }
  return reportedLoader;
}

async function removeCommandShims(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.name === ".bin") {
      await rm(path, { recursive: true, force: true });
    } else if (entry.isDirectory() && !entry.isSymbolicLink()) {
      await removeCommandShims(path);
    }
  }
}

// Static musl avoids recording the Nix development environment's loader.
function hostTarget() {
  const verbose = capture(rustc, ["-vV"]);
  const triple = verbose.match(/^host: (.+)$/m)?.[1];
  if (!triple) throw new Error("rustc did not report its host target");
  if (process.platform !== "linux") return triple;
  const musl = {
    "x86_64-unknown-linux-gnu": "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu": "aarch64-unknown-linux-musl",
  }[triple];
  if (!musl) throw new Error(`no portable headless target is configured for ${triple}`);
  return musl;
}

function assertHeadlessRustGraph(target) {
  const graph = capture(cargo, [
    "tree",
    "--package",
    "metis",
    "--no-default-features",
    "--edges",
    "normal",
    "--target",
    target,
  ]);
  if (/gpui|tree[-_]sitter|wayland|x11|vulkan/iu.test(graph)) {
    throw new Error(`the headless Rust dependency graph contains display code:\n${graph}`);
  }
}

async function copyNodeLicense(licenseDirectory) {
  const installRoot = dirname(dirname(process.execPath));
  const candidates = [
    join(installRoot, "LICENSE"),
    join(installRoot, "share", "doc", "nodejs", "copyright"),
    "/usr/share/doc/nodejs/copyright",
  ];
  for (const candidate of candidates) {
    try {
      await copyFile(candidate, join(licenseDirectory, "Node.js-LICENSE"));
      return;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
  }
  throw new Error(`cannot locate the Node.js license beside ${process.execPath}`);
}

async function writeRuntimeProvenance(packageDirectory, libraryPaths) {
  const outsideNix = [...libraryPaths].filter((path) => !path.startsWith("/nix/store/"));
  if (outsideNix.length > 0) {
    throw new Error(`portable runtime inputs must come from the pinned Nix shell: ${outsideNix}`);
  }
  const flake = JSON.parse(await readFile(join(workspace, "flake.lock"), "utf8"));
  const revision = flake.nodes.nixpkgs.locked.rev;
  if (!revision) throw new Error("flake.lock omitted the nixpkgs revision");
  const outputs = [
    ...new Set(
      [...libraryPaths].map((path) => path.match(/^\/nix\/store\/[^/]+/u)?.[0]),
    ),
  ].filter(Boolean).sort();
  await writeFile(
    join(packageDirectory, "licenses", "runtime-SOURCES.txt"),
    `The bundled glibc and GCC runtime libraries were built from the following Nixpkgs source and its patches:\nhttps://github.com/NixOS/nixpkgs/archive/${revision}.tar.gz\n\nRuntime package outputs:\n${
      outputs.join("\n")
    }\n`,
  );
}

async function copyPortableLinuxNode(packageDirectory) {
  const runtimeDirectory = join(packageDirectory, "lib", "node-runtime");
  await mkdir(runtimeDirectory, { recursive: true });
  const libraryPaths = new Set();
  const loaderPath = await collectNativeDependencies(
    process.execPath,
    packageDirectory,
    libraryPaths,
  );
  if (!loaderPath) throw new Error("ldd did not report the Node.js program loader");
  const modulesDirectory = join(packageDirectory, "lib", "metis-host", "node_modules");
  for (const binary of await nativeFiles(modulesDirectory)) {
    await collectNativeDependencies(binary, packageDirectory, libraryPaths, loaderPath);
  }
  let loaderName;
  for (const path of libraryPaths) {
    // Resolve symlinks for reading, but retain the SONAME used by the loader.
    const name = basename(path);
    if (name.startsWith("ld-linux") || name.startsWith("ld64")) loaderName = name;
    await copyFile(await realpath(path), join(runtimeDirectory, name));
  }
  if (!loaderName) throw new Error("ldd did not report the Node.js program loader");
  await writeRuntimeProvenance(packageDirectory, libraryPaths);
  await copyFile(await realpath(process.execPath), join(runtimeDirectory, "node"));
  await chmod(join(runtimeDirectory, "node"), 0o755);
  await chmod(join(runtimeDirectory, loaderName), 0o755);

  const nodeLauncher = join(packageDirectory, "bin", "node");
  await writeFile(
    nodeLauncher,
    `#!/bin/sh
set -eu
script_directory=\${0%/*}
if [ "$script_directory" = "$0" ]; then script_directory=.; fi
root=$(CDPATH= cd -- "$script_directory/.." && pwd)
exec "$root/lib/node-runtime/${loaderName}" --library-path "$root/lib/node-runtime" "$root/lib/node-runtime/node" "$@"
`,
  );
  await chmod(nodeLauncher, 0o755);
}

async function copyNode(packageDirectory, licenseDirectory) {
  if (process.platform === "linux") {
    await copyPortableLinuxNode(packageDirectory);
  } else {
    const destination = join(packageDirectory, "bin", `node${executableSuffix}`);
    await copyFile(process.execPath, destination);
    if (process.platform !== "win32") await chmod(destination, 0o755);
  }
  await copyNodeLicense(licenseDirectory);
}

async function main() {
  run(process.execPath, ["scripts/check-node-version.mjs"]);
  const target = hostTarget();
  assertHeadlessRustGraph(target);
  const metadata = JSON.parse(capture(cargo, ["metadata", "--no-deps", "--format-version", "1"]));
  const version = metadata.packages.find((pkg) => pkg.name === "metis")?.version;
  if (!version) throw new Error("cargo metadata omitted the metis package");

  const outputDirectory = join(workspace, "target", "headless");
  const packageName = `metis-headless-${version}-${target}`;
  const packageDirectory = join(outputDirectory, packageName);
  await rm(packageDirectory, { recursive: true, force: true });
  await mkdir(join(packageDirectory, "bin"), { recursive: true });
  await mkdir(join(packageDirectory, "lib"), { recursive: true });
  const licenseDirectory = join(packageDirectory, "licenses");
  await mkdir(licenseDirectory, { recursive: true });

  run(cargo, [
    "build",
    "--package",
    "metis",
    "--no-default-features",
    "--release",
    "--target",
    target,
  ]);
  await copyFile(
    join(workspace, "target", target, "release", `metis${executableSuffix}`),
    join(packageDirectory, "bin", `metis${executableSuffix}`),
  );
  for (const name of ["LICENSE-APACHE", "LICENSE-MIT"]) {
    await copyFile(join(workspace, name), join(licenseDirectory, `Metis-${name}`));
  }

  run(pnpm, ["--filter", "@metis/dsh-host", "build"]);
  const deployDirectory = join(packageDirectory, "lib", "metis-host");
  run(pnpm, ["--filter", "@metis/dsh-host", "deploy", "--prod", deployDirectory]);
  const pruning = await pruneProductionPackages(deployDirectory);
  for (
    const path of [
      "pnpm-lock.yaml",
      "pnpm-workspace.yaml",
      "node_modules/.modules.yaml",
      "node_modules/.pnpm-workspace-state-v1.json",
    ]
  ) {
    await rm(join(deployDirectory, path), { force: true });
  }
  await removeCommandShims(join(deployDirectory, "node_modules"));
  await copyNode(packageDirectory, licenseDirectory);
  if (process.platform === "linux") {
    for (const name of await readdir(join(workspace, "licenses", "runtime"))) {
      await copyFile(
        join(workspace, "licenses", "runtime", name),
        join(licenseDirectory, `runtime-${name}`),
      );
    }
  }

  const archive = join(outputDirectory, `${packageName}.tar.gz`);
  await rm(archive, { force: true });
  run("tar", ["-czf", archive, "-C", outputDirectory, packageName]);
  await writeFile(
    join(outputDirectory, "latest.json"),
    `${
      JSON.stringify(
        {
          archive: basename(archive),
          directory: packageName,
          target,
        },
        null,
        2,
      )
    }\n`,
  );
  const archiveStat = await stat(archive);
  console.log(`production packages: ${pruning.kept} kept, ${pruning.removed} pruned`);
  console.log(`headless package: ${archive} (${archiveStat.size} bytes)`);
}

await main();
