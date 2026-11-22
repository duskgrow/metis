import { lstat, readdir, readFile, realpath, rm, stat } from "node:fs/promises";
import { dirname, isAbsolute, join, relative, sep } from "node:path";

function contained(root, candidate) {
  const path = relative(root, candidate);
  return path === "" || (!isAbsolute(path) && path !== ".." && !path.startsWith(`..${sep}`));
}

async function readManifest(packageDirectory) {
  return JSON.parse(await readFile(join(packageDirectory, "package.json"), "utf8"));
}

function dependencyNames(manifest) {
  return new Set([
    ...Object.keys(manifest.dependencies ?? {}),
    ...Object.keys(manifest.optionalDependencies ?? {}),
    ...Object.keys(manifest.peerDependencies ?? {}),
  ]);
}

function isOptionalDependency(manifest, name) {
  if (Object.hasOwn(manifest.dependencies ?? {}, name)) return false;
  return Object.hasOwn(manifest.optionalDependencies ?? {}, name)
    || manifest.peerDependenciesMeta?.[name]?.optional === true;
}

async function installedPackages(virtualEntry) {
  const modulesDirectory = join(virtualEntry, "node_modules");
  const packages = [];
  for (const entry of await readdir(modulesDirectory, { withFileTypes: true })) {
    if (entry.name.startsWith(".") || entry.isSymbolicLink() || !entry.isDirectory()) continue;
    const path = join(modulesDirectory, entry.name);
    if (entry.name.startsWith("@")) {
      for (const scoped of await readdir(path, { withFileTypes: true })) {
        if (scoped.isSymbolicLink() || !scoped.isDirectory()) continue;
        const candidate = join(path, scoped.name);
        try {
          await lstat(join(candidate, "package.json"));
          packages.push(candidate);
        } catch (error) {
          if (error.code !== "ENOENT") throw error;
        }
      }
    } else {
      try {
        await lstat(join(path, "package.json"));
        packages.push(path);
      } catch (error) {
        if (error.code !== "ENOENT") throw error;
      }
    }
  }
  return packages;
}

async function removeBrokenLinks(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isSymbolicLink()) {
      try {
        await stat(path);
      } catch (error) {
        if (error.code !== "ENOENT") throw error;
        await rm(path, { force: true });
      }
    } else if (entry.isDirectory()) {
      await removeBrokenLinks(path);
    }
  }
}

/**
 * Prune a pnpm production deploy to packages reachable from the deployed
 * package's own manifests. Patched manifests therefore control the runtime
 * graph even though pnpm resolves a patch after constructing its lockfile.
 */
export async function pruneProductionPackages(deployDirectory) {
  const rootDirectory = await realpath(deployDirectory);
  const modulesDirectory = join(rootDirectory, "node_modules");
  // Keep .pnpm lexical to the canonical deploy root: resolving a linked store
  // would widen the recursive deletion boundary.
  const virtualDirectory = join(modulesDirectory, ".pnpm");
  const reachable = new Set();

  async function visit(link, name, dependent, optional) {
    let packageDirectory;
    try {
      packageDirectory = await realpath(link);
    } catch (error) {
      if (error.code === "ENOENT" && optional) return;
      if (error.code === "ENOENT") {
        throw new Error(`${dependent} is missing required production dependency ${name}`);
      }
      throw error;
    }
    if (!contained(virtualDirectory, packageDirectory) || reachable.has(packageDirectory)) return;
    const manifest = await readManifest(packageDirectory);
    reachable.add(packageDirectory);
    const dependencyRoot = manifest.name.startsWith("@")
      ? dirname(dirname(packageDirectory))
      : dirname(packageDirectory);
    for (const name of dependencyNames(manifest)) {
      await visit(
        join(dependencyRoot, name),
        name,
        manifest.name,
        isOptionalDependency(manifest, name),
      );
    }
  }

  const rootManifest = await readManifest(rootDirectory);
  for (const name of dependencyNames(rootManifest)) {
    await visit(
      join(modulesDirectory, name),
      name,
      rootManifest.name,
      isOptionalDependency(rootManifest, name),
    );
  }
  if (reachable.size === 0) throw new Error("the production deploy has no reachable packages");

  let removed = 0;
  for (const entry of await readdir(virtualDirectory, { withFileTypes: true })) {
    if (!entry.isDirectory() || entry.name === "node_modules") continue;
    const virtualEntry = join(virtualDirectory, entry.name);
    const installed = await installedPackages(virtualEntry);
    if (installed.length === 0) throw new Error(`cannot identify package in ${virtualEntry}`);
    const keep = await Promise.all(installed.map(async (path) => reachable.has(await realpath(path))));
    if (!keep.some(Boolean)) {
      await rm(virtualEntry, { recursive: true, force: true });
      removed += 1;
    }
  }
  await removeBrokenLinks(modulesDirectory);
  return { kept: reachable.size, removed };
}
