import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const hostPath = fileURLToPath(new URL("../lib/host.js", import.meta.url));

async function launch(t) {
  const home = await mkdtemp(join(tmpdir(), "metis-host-"));
  const child = spawn(process.execPath, [hostPath], {
    env: { ...process.env, DSH_HOME: home },
    stdio: ["pipe", "pipe", "pipe"],
  });
  let logs = "";
  let input = "";
  let readyResolve;
  const ready = new Promise((resolve) => {
    readyResolve = resolve;
  });
  const exited = new Promise((resolve) => {
    child.once("error", (error) => resolve({ error }));
    child.once("close", (code, signal) => resolve({ code, signal }));
  });

  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => {
    logs += chunk;
  });
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    input += chunk;
    let newline = input.indexOf("\n");
    while (newline >= 0) {
      const line = input.slice(0, newline).replace(/\r$/u, "");
      input = input.slice(newline + 1);
      if (line.startsWith("METIS_READY\t")) {
        const [, webUrl, authUrl] = line.split("\t");
        readyResolve({ webUrl, authUrl });
      } else {
        logs += `${line}\n`;
      }
      newline = input.indexOf("\n");
    }
  });

  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGKILL");
    }
    await exited;
    await rm(home, { recursive: true, force: true });
  });
  return { child, exited, home, logs: () => logs, ready };
}

async function waitReady(host) {
  const result = await Promise.race([
    host.ready,
    host.exited.then((status) => ({ status })),
  ]);
  assert.ok(
    result.webUrl,
    `host exited before ready: ${JSON.stringify(result)}\n${host.logs()}`,
  );
  return result;
}

async function postRpc(webUrl, cookie, method, request) {
  const rpcId = `metis-host-test:${method}`;
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
  assert.equal(envelope.type, "server-response");
  assert.equal(envelope.rpcId, rpcId);
  return envelope;
}

test("the private host boots public dsh capabilities without a frontend", {
  timeout: 30_000,
}, async (t) => {
  const host = await launch(t);
  const { webUrl, authUrl } = await waitReady(host);
  const url = new URL(webUrl);
  assert.equal(url.protocol, "http:");
  assert.equal(url.hostname, "127.0.0.1");
  assert.ok(Number(url.port) > 0);
  assert.equal(new URL(authUrl).pathname, "/connect");
  assert.match(host.logs(), /\[metis\] host plugin loaded/u);

  const connect = await fetch(authUrl, {
    redirect: "manual",
    signal: AbortSignal.timeout(10_000),
  });
  assert.equal(connect.status, 302);
  const setCookie = connect.headers.get("set-cookie");
  assert.ok(setCookie);
  const cookie = setCookie.split(";", 1)[0];
  assert.ok(cookie.includes("="));

  const opened = await postRpc(webUrl, cookie, "project/open", {
    path: host.home,
  });
  assert.equal(opened.result.ok, true, JSON.stringify(opened.result));
  assert.equal(opened.result.value.path, await realpath(host.home));
  assert.equal(typeof opened.result.value.projectId, "string");

  const read = await postRpc(webUrl, cookie, "project/read", {
    projectId: opened.result.value.projectId,
    path: "host-profile/package.json",
  });
  assert.equal(read.result.ok, true, JSON.stringify(read.result));
  assert.match(read.result.value.content, /metis-private-host/u);

  const created = await postRpc(webUrl, cookie, "session/create", {
    workspaceId: opened.result.value.projectId,
  });
  assert.equal(created.result.ok, true, JSON.stringify(created.result));
  assert.equal(typeof created.result.value.sessionId, "string");

  const cancelled = await postRpc(webUrl, cookie, "session/cancel", {
    sessionId: created.result.value.sessionId,
  });
  assert.deepEqual(cancelled.result, { ok: true, value: { accepted: true } });

  const invalidAuth = new URL("/connect", webUrl);
  invalidAuth.searchParams.set("token", "A".repeat(43));
  const rejected = await fetch(invalidAuth, { redirect: "manual" });
  assert.equal(rejected.status, 401);

  const index = await fetch(webUrl);
  assert.equal(index.status, 404);
  assert.equal(await index.text(), "");

  host.child.stdin.write("METIS_SHUTDOWN\n");
  const status = await host.exited;
  assert.deepEqual(status, { code: 0, signal: null });
  await assert.rejects(fetch(webUrl, { signal: AbortSignal.timeout(1_000) }));
});

test("closing the watchdog pipe during boot stops the host before readiness", {
  timeout: 30_000,
}, async (t) => {
  const host = await launch(t);
  host.child.stdin.end();
  const result = await Promise.race([host.ready, host.exited]);
  assert.deepEqual(result, { code: 0, signal: null }, host.logs());
});

test("closing the watchdog pipe stops the host after readiness", {
  timeout: 30_000,
}, async (t) => {
  const host = await launch(t);
  const { webUrl } = await waitReady(host);
  host.child.stdin.end();
  const status = await host.exited;
  assert.deepEqual(status, { code: 0, signal: null });
  await assert.rejects(fetch(webUrl, { signal: AbortSignal.timeout(1_000) }));
});
