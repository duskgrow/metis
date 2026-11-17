import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { loadMarker } from "../lib/index.js";

const require = createRequire(import.meta.url);
const dshBin = resolve(dirname(require.resolve("@deepseek-ai/dsh/package.json")), "lib/bin.js");
const pluginPath = fileURLToPath(new URL("../lib/index.js", import.meta.url));

test("the pinned dsh loads the compiled plugin and starts its web runtime", {
  timeout: 60_000,
}, async () => {
  const home = await mkdtemp(join(tmpdir(), "metis-dsh-"));
  const patchPath = join(home, "plugin.json");
  // JSON is valid YAML, so the test needs no YAML serializer dependency.
  await writeFile(
    patchPath,
    JSON.stringify([
      { insert: [{ id: "metis-host", name: pluginPath }] },
    ]),
  );

  // Spawn Node directly instead of a package-manager wrapper so cleanup always
  // terminates the process that owns the web server.
  const child = spawn(process.execPath, [
    dshBin,
    "web",
    "--patch",
    patchPath,
    "--host",
    "127.0.0.1",
    "--port",
    "0",
    "--no-open",
  ], {
    env: { ...process.env, DSH_HOME: join(home, "home") },
    stdio: ["ignore", "pipe", "pipe"],
  });

  let output = "";
  let pluginLoaded = false;
  let webUrl = "";
  const ready = new Promise((resolveReady) => {
    const collect = (chunk) => {
      const incoming = output + chunk.toString();
      pluginLoaded ||= incoming.includes(loadMarker);
      webUrl ||= incoming.match(/dsh web: http:\/\/127\.0\.0\.1:\d+\//)?.[0] ?? "";
      output = incoming.slice(-64 * 1024);
      if (pluginLoaded && webUrl) resolveReady();
    };
    child.stdout.on("data", collect);
    child.stderr.on("data", collect);
  });
  const exited = new Promise((resolveExit) => {
    child.once("error", (error) => resolveExit({ error }));
    child.once("exit", (code, signal) => resolveExit({ code, signal }));
  });
  const timeout = setTimeout(() => child.kill(), 30_000);

  try {
    const result = await Promise.race([ready.then(() => null), exited]);
    assert.equal(result, null, `dsh exited before becoming ready: ${output}`);
    assert.match(webUrl, /^dsh web: http:\/\/127\.0\.0\.1:\d+\/$/);
  } finally {
    clearTimeout(timeout);
    if (child.exitCode === null && child.signalCode === null) child.kill();
    await exited;
    await rm(home, { recursive: true, force: true });
  }
});
