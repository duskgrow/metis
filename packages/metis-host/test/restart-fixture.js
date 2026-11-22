import { writeFileSync } from "node:fs";
import { join } from "node:path";

const provider = "metis-controlled";
const model = "controlled";
const restartMarker = "METIS_RESTART_LIVE";
const outputMarker = "METIS_LIVE_CHUNK";

function waitForAbort(signal) {
  if (signal.aborted) return Promise.reject(signal.reason);
  return new Promise((_, reject) => {
    signal.addEventListener("abort", () => reject(signal.reason), { once: true });
  });
}

// Keep the public LlmAdapter defaults local to this fixture so testing the Host
// composition does not require another production package dependency.
class ControlledAdapter {
  providerInfo(id) {
    return { id, name: id };
  }

  providerRetryPolicy() {
    return undefined;
  }

  imageRequestPricing() {
    return undefined;
  }

  imageRequestTarget(_provider, _model, image) {
    return { width: image.width, height: image.height, maxBytes: image.bytes };
  }

  listModels() {
    return Promise.resolve([]);
  }

  resolveModel(providerId, modelId) {
    return Promise.resolve({ provider: providerId, id: modelId, name: modelId });
  }

  async prepareCall(providerId, modelId, signal) {
    return {
      model: await this.resolveModel(providerId, modelId, signal),
      stream: (options) => this.stream(options),
    };
  }

  async *stream(options) {
    if (options.purpose !== undefined) {
      yield { type: "finish", reason: { kind: "stop" } };
      return;
    }
    yield { type: "block-start", index: 0, blockType: "text" };
    yield { type: "text-delta", index: 0, text: outputMarker };
    console.log("[metis-test] assistant stream held open");
    await waitForAbort(options.signal);
  }
}

function reportJobError(agent, error) {
  writeFileSync(
    join(agent.session.header.cwd, "metis-test-job.error"),
    String(error),
  );
}

function shellQuote(value) {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

function startBashJob(ctx, agent) {
  let proc;
  const pidPath = shellQuote(join(agent.session.header.cwd, "metis-test-job.pid"));
  const cwdPath = shellQuote(join(agent.session.header.cwd, "metis-test-job.cwd"));
  const spec = ctx.shell.resolve({
    command: `printf 'METIS_JOB_STARTED\\n'; pwd > ${cwdPath}; printf '%s' "$$" > ${pidPath}; exec sleep 30`,
    workdir: agent.session.header.cwd,
    onExpiry: "none",
  });
  const id = ctx.jobs.start({
    kind: "bash",
    label: "metis restart fixture",
    owner: agent.id,
    output: ["stdout", "stderr"].map((channel) => ({
      channel,
      read: (fromByte) =>
        proc === undefined
          ? { text: "", nextOffset: fromByte, lossy: false }
          : proc.observed[channel].readFrom(fromByte),
    })),
    run() {
      const abort = new AbortController();
      const done = (async () => {
        try {
          proc = await ctx.shell.execute({ ...spec, signal: abort.signal });
          console.log("[metis-test] bash process started");
          if (abort.signal.aborted) proc.kill();
          await proc.done;
          writeFileSync(
            join(agent.session.header.cwd, "metis-test-job.done"),
            JSON.stringify({
              status: proc.status,
              exitCode: proc.exitCode,
              signal: proc.signal,
              stdout: proc.observed.stdout.readFrom(0).text,
              stderr: proc.observed.stderr.readFrom(0).text,
            }),
          );
          return proc.status === "killed"
            ? { status: "killed", detail: proc.signal ?? "killed" }
            : { status: "completed", detail: `exit code: ${proc.exitCode ?? 0}` };
        } catch (error) {
          if (!abort.signal.aborted) reportJobError(agent, error);
          return {
            status: abort.signal.aborted ? "killed" : "failed",
            detail: String(error),
          };
        }
      })();
      return {
        done,
        cancel(reason) {
          if (abort.signal.aborted) return;
          abort.abort(reason);
          proc?.kill();
        },
      };
    },
  });
  console.log(`[metis-test] job started ${id}`);
}

export const inject = ["llm", "shell", "jobs"];

export function apply(ctx) {
  writeFileSync(
    join(process.env.DSH_HOME, "metis-test-host.pid"),
    String(process.pid),
  );
  ctx.llm.registerAdapter([provider], new ControlledAdapter());
  ctx.on("agent/created", ({ agent }) => {
    const jobs = ctx.jobs.list(agent.id).map((job) => job.id);
    console.log(`[metis-test] jobs ${agent.id} ${JSON.stringify(jobs)}`);
    agent.model.select({ provider, model });
    let started = false;
    agent.ctx.on("session/event", (session, event) => {
      if (started || event.type !== "user/message" || !Array.isArray(event.data.content)) {
        return;
      }
      const text = event.data.content
        .filter((part) => part.type === "text")
        .map((part) => part.text)
        .join("\n");
      if (!text.includes(restartMarker)) return;
      started = true;
      Promise.resolve()
        .then(() => startBashJob(ctx, agent))
        .catch((error) => {
          reportJobError(agent, error);
          console.log(`[metis-test] job failed: ${String(error)}`);
        });
    });
  }, { global: true });
}
