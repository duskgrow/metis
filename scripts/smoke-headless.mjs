import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import {
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { nativeFiles } from "./native-files.mjs";

const workspace = fileURLToPath(new URL("..", import.meta.url));
const outputDirectory = join(workspace, "target", "headless");
const packageArgument = process.argv.indexOf("--package-dir");
const suppliedPackage = packageArgument === -1 ? undefined : process.argv[packageArgument + 1];
if (packageArgument !== -1 && !suppliedPackage) {
  throw new Error("--package-dir requires a path");
}
const latest = suppliedPackage
  ? undefined
  : JSON.parse(readFileSync(join(outputDirectory, "latest.json"), "utf8"));
const COMMAND_TIMEOUT = 30_000;
const MAX_LOG_BYTES = 64 * 1024;

function run(command, args, options = {}) {
  execFileSync(command, args, {
    cwd: workspace,
    stdio: "inherit",
    timeout: COMMAND_TIMEOUT,
    ...options,
  });
}

function capture(command, args, options = {}) {
  return execFileSync(command, args, {
    cwd: workspace,
    encoding: "utf8",
    timeout: COMMAND_TIMEOUT,
    ...options,
  });
}

function contained(root, candidate) {
  const path = relative(root, candidate);
  return path === "" || (!isAbsolute(path) && path !== ".." && !path.startsWith(`..${sep}`));
}

function walk(directory, visit) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    visit(path, entry);
    if (entry.isDirectory() && !entry.isSymbolicLink()) walk(path, visit);
  }
}

function cleanEnvironment(home) {
  const environment = {
    ...process.env,
    HOME: home,
    PATH: "/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin",
  };
  for (
    const name of [
      "DISPLAY",
      "WAYLAND_DISPLAY",
      "DSH_HOME",
      "LD_LIBRARY_PATH",
      "NIX_LD",
      "NIX_LD_LIBRARY_PATH",
      "NODE_OPTIONS",
      "XDG_CACHE_HOME",
      "XDG_CONFIG_HOME",
      "XDG_DATA_HOME",
      "XDG_STATE_HOME",
    ]
  ) {
    delete environment[name];
  }
  return environment;
}

function assertPortableBinary(packageDirectory) {
  if (process.platform !== "linux") return;
  const result = capture("sh", [
    "-c",
    "ldd \"$1\" 2>&1 || true",
    "sh",
    join(packageDirectory, "bin", "metis"),
  ]);
  assert.match(result, /statically linked|not a dynamic executable/u, result);
}

function assertNoDisplayPackages(packageDirectory) {
  const forbidden = /gpui|tree[-_]sitter|dsh-web-(?:app|frontend)|libreoffice|electron|wayland|vulkan|libx11/iu;
  walk(packageDirectory, (path) => {
    assert.doesNotMatch(path.toLowerCase(), forbidden, `display dependency in ${path}`);
  });
  assert.equal(existsSync(join(packageDirectory, "lib", "metis-host", "src")), false);
  assert.equal(existsSync(join(packageDirectory, "lib", "metis-host", "test")), false);
}

function assertRelocatable(packageDirectory) {
  const textExtensions = [".js", ".mjs", ".cjs", ".json", ".map", ".sh", ".cmd", ".ps1"];
  walk(packageDirectory, (path, entry) => {
    assert.notEqual(entry.name, ".bin", `generated command shim in ${path}`);
    if (entry.isSymbolicLink()) {
      assert.equal(isAbsolute(readlinkSync(path)), false, `absolute symlink in ${path}`);
    }
    const source = lstatSync(path);
    if (
      source.isFile()
      && textExtensions.some((extension) => entry.name.endsWith(extension))
      && !contained(join(packageDirectory, "licenses"), path)
    ) {
      const contents = readFileSync(path, "utf8");
      const nixPath = contents.match(/\/nix\/store\/[a-z0-9]{32}/u)?.[0];
      assert.equal(nixPath, undefined, `build-machine path ${nixPath} in ${path}`);
      if (suppliedPackage === undefined) {
        assert.equal(
          contents.includes(workspace),
          false,
          `workspace path in ${path}`,
        );
      }
    }
  });
}

async function assertNativeClosure(packageDirectory) {
  if (process.platform !== "linux") return;
  const runtime = join(packageDirectory, "lib", "node-runtime");
  const loaderName = readdirSync(runtime).find((name) => name.startsWith("ld-linux"));
  assert.ok(loaderName, "the package carries its Linux program loader");
  const loader = join(runtime, loaderName);
  const binaries = [
    join(runtime, "node"),
    ...await nativeFiles(join(packageDirectory, "lib", "metis-host", "node_modules")),
  ];
  for (const binary of binaries) {
    const result = capture(loader, ["--list", "--library-path", runtime, binary]);
    assert.doesNotMatch(result, /=> not found/u, `unresolved dependency in ${binary}`);
    for (const line of result.split("\n")) {
      const dependency = line.match(/=>\s*(\/[^\s]+)/u)?.[1];
      if (dependency) {
        assert.ok(
          contained(packageDirectory, dependency),
          `${binary} uses external dependency ${dependency}`,
        );
      }
    }
  }
}

function virtualPackage(packageDirectory, name) {
  const virtual = join(packageDirectory, "lib", "metis-host", "node_modules", ".pnpm");
  const entry = readdirSync(virtual).find((entry) => entry.startsWith(`${name}@`));
  assert.ok(entry, `the package carries ${name}`);
  return join(virtual, entry, "node_modules", name);
}

async function postRpc(webUrl, cookie, method, request) {
  const rpcId = `metis-package-smoke:${method}`;
  const response = await fetch(new URL(`/api/${method}`, webUrl), {
    method: "POST",
    headers: { "content-type": "application/json", cookie },
    body: JSON.stringify({
      type: "client-request",
      rpcId,
      method,
      payload: { args: { request } },
    }),
    signal: AbortSignal.timeout(10_000),
  });
  assert.equal(response.status, 200);
  const envelope = await response.json();
  assert.equal(envelope.rpcId, rpcId);
  return envelope;
}

async function main() {
  const scratch = mkdtempSync(join(tmpdir(), "metis-headless-package-"));
  let child;
  let exited;
  let logs = "";
  try {
    if (suppliedPackage === undefined) {
      run("tar", ["-xzf", join(outputDirectory, latest.archive), "-C", scratch]);
    }
    const packageDirectory = suppliedPackage === undefined
      ? join(scratch, latest.directory)
      : resolve(suppliedPackage);
    assertNoDisplayPackages(packageDirectory);
    assertRelocatable(packageDirectory);
    assertPortableBinary(packageDirectory);
    await assertNativeClosure(packageDirectory);

    const home = join(scratch, "user-home");
    mkdirSync(home, { recursive: true });
    const environment = cleanEnvironment(home);
    const executable = join(packageDirectory, "bin", "metis");
    const node = join(packageDirectory, "bin", "node");
    const nodeVersion = capture(node, ["--version"], { env: environment });
    assert.match(nodeVersion, /^v\d+\./u);
    capture(node, [
      "-e",
      "for (const path of process.argv.slice(1)) require(path);",
      virtualPackage(packageDirectory, "node-pty"),
      virtualPackage(packageDirectory, "sharp"),
    ], { env: environment });
    run(executable, ["--version"], { env: environment });

    const hooks = join(scratch, "empty-hooks");
    const templates = join(scratch, "empty-templates");
    mkdirSync(hooks);
    mkdirSync(templates);
    const gitEnvironment = {
      ...environment,
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_TEMPLATE_DIR: templates,
      GIT_TERMINAL_PROMPT: "0",
    };
    const git = (args) =>
      run("git", [
        "-c",
        "commit.gpgsign=false",
        "-c",
        `core.hooksPath=${hooks}`,
        ...args,
      ], { env: gitEnvironment });
    const project = join(scratch, "project");
    git(["init", "--quiet", project]);
    git(["-C", project, "config", "user.name", "Metis Package Smoke"]);
    git(["-C", project, "config", "user.email", "metis@example.invalid"]);
    writeFileSync(join(project, "tracked.txt"), "before\n");
    git(["-C", project, "add", "--", "."]);
    git(["-C", project, "commit", "--quiet", "-m", "initial"]);
    writeFileSync(join(project, "tracked.txt"), "after\n");

    const state = join(scratch, "state");
    child = spawn(executable, ["host", "--home", state], {
      env: environment,
      stdio: ["ignore", "pipe", "pipe"],
    });
    exited = new Promise((resolve) => {
      child.once("error", (error) => resolve({ error }));
      child.once("close", (code, signal) => resolve({ code, signal }));
    });
    let readyResolve;
    let readyReject;
    const ready = new Promise((resolve, reject) => {
      readyResolve = resolve;
      readyReject = reject;
    });
    let startupTimer;
    let shutdownTimer;
    let readinessBuffer = "";
    let readySeen = false;
    const appendLog = (chunk) => {
      logs = (logs + chunk).slice(-MAX_LOG_BYTES);
    };
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      appendLog(chunk);
      if (readySeen) return;
      readinessBuffer += chunk;
      const newline = readinessBuffer.indexOf("\n");
      if (newline >= 0) {
        const line = readinessBuffer.slice(0, newline).replace(/\r$/u, "");
        readySeen = true;
        if (basename(line) === "connection.json") {
          readyResolve({ connectionFile: line });
        } else {
          readyReject(new Error(`unexpected Host readiness output: ${line}`));
        }
      } else if (readinessBuffer.length > 4096) {
        readyReject(new Error("Host readiness path exceeded its limit"));
      }
    });
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", appendLog);
    const startup = await Promise.race([
      ready,
      exited.then((status) => ({ status })),
      new Promise((resolve) => {
        startupTimer = setTimeout(() => resolve({ timeout: true }), 30_000);
      }),
    ]);
    clearTimeout(startupTimer);
    assert.ok(
      startup.connectionFile,
      `Host did not publish a connection file: ${JSON.stringify(startup)}\n${logs}`,
    );

    const connectionFile = startup.connectionFile;
    assert.equal(connectionFile, join(state, "connection.json"));
    const connection = JSON.parse(readFileSync(connectionFile, "utf8"));
    if (process.platform !== "win32") {
      assert.equal(statSync(connectionFile).mode & 0o777, 0o600);
    }
    const connect = await fetch(connection.authUrl, {
      redirect: "manual",
      signal: AbortSignal.timeout(10_000),
    });
    assert.equal(connect.status, 302);
    const cookie = connect.headers.get("set-cookie")?.split(";", 1)[0];
    assert.ok(cookie);

    const opened = await postRpc(connection.webUrl, cookie, "project/open", { path: project });
    assert.equal(opened.result.ok, true, JSON.stringify(opened.result));
    const projectId = opened.result.value.projectId;
    const status = await postRpc(connection.webUrl, cookie, "git/status", { projectId });
    assert.equal(status.result.ok, true, JSON.stringify(status.result));
    assert.match(status.result.value.status, /tracked\.txt/u);
    const diff = await postRpc(connection.webUrl, cookie, "git/diff", { projectId });
    assert.equal(diff.result.ok, true, JSON.stringify(diff.result));
    assert.match(diff.result.value.patch, /\+after/u);
    const session = await postRpc(connection.webUrl, cookie, "session/create", {
      workspaceId: projectId,
    });
    assert.equal(session.result.ok, true, JSON.stringify(session.result));
    const cancelled = await postRpc(connection.webUrl, cookie, "session/cancel", {
      sessionId: session.result.value.sessionId,
    });
    assert.deepEqual(cancelled.result, { ok: true, value: { accepted: true } });

    child.kill("SIGTERM");
    const shutdown = await Promise.race([
      exited,
      new Promise((resolve) => {
        shutdownTimer = setTimeout(() => resolve({ timeout: true }), 20_000);
      }),
    ]);
    clearTimeout(shutdownTimer);
    assert.deepEqual(shutdown, { code: 0, signal: null }, logs);
    assert.equal(existsSync(connectionFile), false);
    await assert.rejects(fetch(connection.webUrl, { signal: AbortSignal.timeout(1_000) }));
    console.log(`headless smoke passed: ${basename(latest?.archive ?? packageDirectory)}`);
  } catch (error) {
    if (logs) console.error(logs);
    throw error;
  } finally {
    if (child && child.exitCode === null && child.signalCode === null) {
      child.kill("SIGKILL");
      let killTimer;
      const reaped = await Promise.race([
        exited,
        new Promise((resolve) => {
          killTimer = setTimeout(() => resolve({ timeout: true }), 5_000);
        }),
      ]);
      clearTimeout(killTimer);
      if (reaped.timeout) throw new Error("failed to reap the killed smoke Host");
    }
    rmSync(scratch, { recursive: true, force: true });
  }
}

await main();
