import { execFile } from "node:child_process";
import { promisify } from "node:util";

import type { Context } from "@deepseek-ai/cordis";
import { Remote, RemoteError, TypertRemoteService } from "@deepseek-ai/dsh-typert-protocol";

import { projectRoot } from "./project.js";
import { decodeText, MAX_TEXT_BYTES } from "./text.js";

const execFileAsync = promisify(execFile);

export interface GitRequest {
  readonly projectId: string;
}

declare module "@deepseek-ai/dsh-typert-protocol" {
  interface RemoteErrorDetailsMap {
    "git/command-failed": {
      readonly projectId: string;
      readonly operation: "status" | "diff";
    };
  }
}

function validRequest(request: GitRequest): boolean {
  return typeof request?.projectId === "string" && request.projectId.length > 0;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function runGit(
  root: string,
  args: readonly string[],
  signal: AbortSignal,
): Promise<Buffer> {
  const result = await execFileAsync(
    "git",
    ["--no-pager", "--no-optional-locks", ...args],
    {
      cwd: root,
      encoding: "buffer",
      env: { ...process.env, GIT_TERMINAL_PROMPT: "0" },
      maxBuffer: MAX_TEXT_BYTES,
      signal,
      windowsHide: true,
    },
  );
  return result.stdout;
}

async function diffBase(root: string, signal: AbortSignal): Promise<string> {
  try {
    return decodeText(
      await runGit(root, ["rev-parse", "--verify", "--quiet", "HEAD^{tree}"], signal),
    ).trim();
  } catch (error: unknown) {
    if (signal.aborted) throw error;
    const emptyDevice = process.platform === "win32" ? "NUL" : "/dev/null";
    return decodeText(
      await runGit(root, ["hash-object", "-t", "tree", emptyDevice], signal),
    ).trim();
  }
}

export class GitService extends TypertRemoteService {
  static inject = ["typertGateway", "workspaceRegistry"];

  constructor(ctx: Context) {
    super(ctx, "metisGit", { namespace: "git" });
  }

  @Remote
  async status(
    request: GitRequest,
    signal: AbortSignal,
  ): Promise<{ readonly status: string }> {
    if (!validRequest(request)) {
      throw new RemoteError(
        "gateway/bad-request",
        "git/status requires projectId",
        {},
      );
    }
    const root = await projectRoot(this.ctx, request.projectId);
    try {
      const status = await runGit(
        root,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all", "--", "."],
        signal,
      );
      return { status: decodeText(status) };
    } catch (error: unknown) {
      throw new RemoteError(
        "git/command-failed",
        `git status failed: ${errorMessage(error)}`,
        { projectId: request.projectId, operation: "status" },
      );
    }
  }

  @Remote
  async diff(
    request: GitRequest,
    signal: AbortSignal,
  ): Promise<{ readonly patch: string }> {
    if (!validRequest(request)) {
      throw new RemoteError(
        "gateway/bad-request",
        "git/diff requires projectId",
        {},
      );
    }
    const root = await projectRoot(this.ctx, request.projectId);
    try {
      const base = await diffBase(root, signal);
      const patch = await runGit(
        root,
        [
          "diff",
          "--no-ext-diff",
          "--no-textconv",
          "--no-color",
          "--binary",
          "--find-renames",
          base,
          "--",
          ".",
        ],
        signal,
      );
      return { patch: decodeText(patch) };
    } catch (error: unknown) {
      throw new RemoteError(
        "git/command-failed",
        `git diff failed: ${errorMessage(error)}`,
        { projectId: request.projectId, operation: "diff" },
      );
    }
  }
}
