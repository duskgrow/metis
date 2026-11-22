import assert from "node:assert/strict";
import { lstat, mkdir, mkdtemp, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";

import { pruneProductionPackages } from "./prune-production.mjs";

async function packageAt(virtualDirectory, key, name, manifest) {
  const directory = join(virtualDirectory, key, "node_modules", name);
  await mkdir(directory, { recursive: true });
  await writeFile(
    join(directory, "package.json"),
    JSON.stringify({ name, version: "1.0.0", ...manifest }),
  );
  return directory;
}

async function link(modulesDirectory, name, target) {
  const path = join(modulesDirectory, name);
  await mkdir(dirname(path), { recursive: true });
  await symlink(target, path, process.platform === "win32" ? "junction" : "dir");
}

test("production pruning follows manifests and removes unreachable virtual packages", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "metis-prune-test-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const modules = join(directory, "node_modules");
  const virtual = join(modules, ".pnpm");
  const wanted = await packageAt(virtual, "a", "@metis/a", {
    dependencies: { b: "1.0.0" },
    optionalDependencies: { optional: "1.0.0", missingOptional: "1.0.0" },
    peerDependencies: { peer: "1.0.0", missingPeer: "1.0.0" },
    peerDependenciesMeta: { missingPeer: { optional: true } },
    devDependencies: { unwanted: "1.0.0" },
  });
  await packageAt(virtual, "b", "b", { dependencies: { c: "1.0.0" } });
  await packageAt(virtual, "c", "c", {});
  await packageAt(virtual, "optional", "optional", {});
  await packageAt(virtual, "peer", "peer", {});
  const unwanted = await packageAt(virtual, "unwanted", "unwanted", {});
  await writeFile(
    join(directory, "package.json"),
    JSON.stringify({
      dependencies: { "@metis/a": "1.0.0" },
    }),
  );
  await link(modules, "@metis/a", wanted);
  await link(join(virtual, "a", "node_modules"), "b", join(virtual, "b", "node_modules", "b"));
  await link(join(virtual, "b", "node_modules"), "c", join(virtual, "c", "node_modules", "c"));
  await link(
    join(virtual, "a", "node_modules"),
    "optional",
    join(virtual, "optional", "node_modules", "optional"),
  );
  await link(
    join(virtual, "a", "node_modules"),
    "peer",
    join(virtual, "peer", "node_modules", "peer"),
  );
  const stale = join(virtual, "a", "node_modules", "unwanted");
  await link(dirname(stale), "unwanted", unwanted);
  await mkdir(join(virtual, "node_modules"));

  assert.deepEqual(await pruneProductionPackages(directory), { kept: 5, removed: 1 });
  assert.deepEqual((await readdir(virtual)).sort(), [
    "a",
    "b",
    "c",
    "node_modules",
    "optional",
    "peer",
  ]);
  await assert.rejects(lstat(stale), { code: "ENOENT" });
  await assert.rejects(lstat(unwanted), { code: "ENOENT" });
});

test("production pruning rejects a missing required dependency", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "metis-prune-missing-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  await mkdir(join(directory, "node_modules", ".pnpm"), { recursive: true });
  await writeFile(
    join(directory, "package.json"),
    JSON.stringify({ name: "root", dependencies: { missing: "1.0.0" } }),
  );
  await assert.rejects(
    pruneProductionPackages(directory),
    /root is missing required production dependency missing/u,
  );
});
