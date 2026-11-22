import { mkdir, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { loadLayeredEnv, loadProfileDirectory, reportSkippedBundles } from "@deepseek-ai/dsh-app-boot";
import type {} from "@deepseek-ai/dsh-client-connection";
import type {} from "@deepseek-ai/dsh-host-webserver";
import { runProfile } from "@deepseek-ai/dsh/profile-boot";

const READY = "METIS_READY";
const SHUTDOWN = "METIS_SHUTDOWN";
const MAX_INPUT_BYTES = 1024;

type RunningProfile = Awaited<ReturnType<typeof runProfile>>;

let running: RunningProfile | undefined;
let stopRequested = false;
let stopping: Promise<void> | undefined;

function writeControl(line: string): Promise<void> {
  return new Promise((resolve) => {
    process.stdout.write(`${line}\n`, () => resolve());
  });
}

function reportFailure(error: unknown): void {
  console.error(error);
  process.exitCode = 1;
  process.stdin.pause();
}

async function stop(): Promise<void> {
  stopRequested = true;
  if (!running) return;
  stopping ??= (async () => {
    // dsh uses this code when disposal fails or times out. Successful disposal
    // reaches the explicit successful exit below.
    await running.shutdown.shutdown(1);
    // A disposed plugin can still own an event-loop handle. Application cleanup
    // is complete, so do not let such a handle extend the Host lifetime.
    process.exit(0);
  })();
  return stopping;
}

function requestStop(): void {
  stopRequested = true;
  if (running) void stop().catch(reportFailure);
}

// stdin is both the command channel and the watchdog lifetime pipe. Install its
// listeners before boot so a parent disappearance during startup is not lost.
let input = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk: string) => {
  input += chunk;
  if (Buffer.byteLength(input) > MAX_INPUT_BYTES) {
    input = "";
    console.error("Metis Host control input exceeded its size limit");
    requestStop();
    return;
  }
  let newline = input.indexOf("\n");
  while (newline >= 0) {
    const line = input.slice(0, newline).replace(/\r$/u, "");
    input = input.slice(newline + 1);
    if (line === SHUTDOWN) requestStop();
    newline = input.indexOf("\n");
  }
});
process.stdin.once("end", requestStop);
process.stdin.once("error", (error) => {
  console.error(error);
  requestStop();
});
process.stdin.resume();

async function main(): Promise<void> {
  const home = process.env.DSH_HOME;
  if (!home) throw new Error("DSH_HOME is required");

  const profileDir = join(resolve(home), "host-profile");
  await mkdir(profileDir, { recursive: true });
  await writeFile(
    join(profileDir, "package.json"),
    `${
      JSON.stringify(
        {
          name: "metis-private-host",
          private: true,
          dsh: { profile: { bundles: ["@deepseek-ai/dsh-base"] } },
        },
        null,
        2,
      )
    }\n`,
  );

  const plugin = fileURLToPath(new URL("./index.js", import.meta.url));
  // Built-in wiring is source-owned and regenerated on every boot. User
  // overrides belong in $DSH_HOME/cordis.patch.yml, which dsh applies later.
  await writeFile(
    join(profileDir, "cordis.patch.yml"),
    `${
      JSON.stringify(
        [
          {
            insert: [
              {
                id: "webserver",
                name: "@deepseek-ai/dsh-host-webserver",
                config: { host: "127.0.0.1", port: 0 },
              },
              {
                id: "connection",
                name: "@deepseek-ai/dsh-client-connection",
                inject: ["webServer"],
              },
              { id: "workspace", name: "@deepseek-ai/dsh-workspace" },
              {
                id: "file-references",
                name: "@deepseek-ai/dsh-file-reference-local",
              },
              {
                id: "file-uploads",
                name: "@deepseek-ai/dsh-client-file-upload",
              },
              {
                id: "session-controller",
                name: "@deepseek-ai/dsh-api-session-controller",
              },
              {
                id: "api-remotes",
                name: "@deepseek-ai/dsh-api-remotes",
              },
              { id: "metis-host", name: plugin },
            ],
          },
        ],
        null,
        2,
      )
    }\n`,
  );

  const installAnchor = createRequire(import.meta.url).resolve(
    "@deepseek-ai/dsh/package.json",
  );
  const profile = loadProfileDirectory("dsh", profileDir, installAnchor);
  reportSkippedBundles("dsh", profile);
  running = await runProfile({
    environment: loadLayeredEnv("dsh"),
    profile: "metis",
    resolvedProfile: { profile, installAnchor },
    patchFiles: [],
    args: [],
  });

  if (stopRequested) {
    await stop();
    return;
  }

  const webUrl = new URL("/", running.ctx.webServer.getOrigin()).href;
  const connectUrl = new URL("/connect", webUrl);
  const authUrl = running.ctx.connection.authenticatedUrl(connectUrl.href);
  await writeControl(`${READY}\t${webUrl}\t${authUrl}`);
}

await main().catch(reportFailure);
