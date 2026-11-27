import { constants } from "node:fs";
import { chmod, mkdtemp, open, realpath, rename, rm, stat } from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";

import type { Context } from "@deepseek-ai/cordis";
import { Remote, RemoteError, TypertRemoteService } from "@deepseek-ai/dsh-typert-protocol";
import { type Workspace, WorkspaceId } from "@deepseek-ai/dsh-workspace";

import { decodeText, MAX_TEXT_BYTES } from "./text.js";

const MAX_RECENT_SESSIONS = 100;

export interface ProjectValue {
  readonly projectId: string;
  readonly path: string;
  readonly title: string;
  readonly recentSessionIds: readonly string[];
  readonly hasMoreSessions: boolean;
}

export interface ProjectOpenRequest {
  readonly path: string;
}

export interface ProjectGetRequest {
  readonly projectId: string;
}

export interface ProjectReadRequest {
  readonly projectId: string;
  readonly path: string;
}

export interface ProjectWriteRequest extends ProjectReadRequest {
  readonly content: string;
}

declare module "@deepseek-ai/dsh-typert-protocol" {
  interface RemoteErrorDetailsMap {
    "project/not-found": { readonly projectId: string };
    "project/open-failed": { readonly path: string };
    "project/unavailable": { readonly projectId: string; readonly path: string };
    "project/path-outside": { readonly projectId: string; readonly path: string };
    "project/read-failed": { readonly projectId: string; readonly path: string };
    "project/write-failed": { readonly projectId: string; readonly path: string };
  }
}

export function projectValue(workspace: Workspace): ProjectValue {
  return {
    projectId: workspace.id,
    path: workspace.path,
    title: workspace.title,
    recentSessionIds: workspace.sessionIds.slice(0, MAX_RECENT_SESSIONS),
    hasMoreSessions: workspace.sessionIds.length > MAX_RECENT_SESSIONS,
  };
}

function workspaceFor(ctx: Context, projectId: string): Workspace {
  const workspace = ctx.workspaceRegistry.get(WorkspaceId(projectId));
  if (workspace === undefined) {
    throw new RemoteError(
      "project/not-found",
      `project "${projectId}" not found`,
      { projectId },
    );
  }
  return workspace;
}

export async function projectRoot(
  ctx: Context,
  projectId: string,
): Promise<string> {
  const workspace = workspaceFor(ctx, projectId);
  try {
    const root = await realpath(workspace.path);
    // A workspace is bound to the canonical directory captured when it opened.
    // Do not silently follow a replacement symlink to a different directory.
    if (root !== workspace.path || !(await stat(root)).isDirectory()) {
      throw new Error("project root changed or is no longer a directory");
    }
    return root;
  } catch (error: unknown) {
    throw new RemoteError(
      "project/unavailable",
      `project "${projectId}" is unavailable: ${errorMessage(error)}`,
      { projectId, path: workspace.path },
    );
  }
}

function contained(root: string, candidate: string): boolean {
  const path = relative(root, candidate);
  return path === ""
    || (!isAbsolute(path) && path !== ".." && !path.startsWith(`..${sep}`));
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function existingProjectFile(
  ctx: Context,
  projectId: string,
  path: string,
  failureCode: "project/read-failed" | "project/write-failed",
): Promise<string> {
  const root = await projectRoot(ctx, projectId);
  const candidate = resolve(root, path);
  if (isAbsolute(path) || !contained(root, candidate)) {
    throw new RemoteError(
      "project/path-outside",
      `path "${path}" is outside project "${projectId}"`,
      { projectId, path },
    );
  }

  let canonical: string;
  try {
    canonical = await realpath(candidate);
  } catch (error: unknown) {
    throw new RemoteError(
      failureCode,
      `cannot resolve project file: ${errorMessage(error)}`,
      { projectId, path },
    );
  }
  if (!contained(root, canonical)) {
    throw new RemoteError(
      "project/path-outside",
      `path "${path}" resolves outside project "${projectId}"`,
      { projectId, path },
    );
  }
  return canonical;
}

export class ProjectService extends TypertRemoteService {
  static inject = ["typertGateway", "workspaceRegistry"];

  constructor(ctx: Context) {
    super(ctx, "metisProject", { namespace: "project" });
  }

  @Remote
  async open(request: ProjectOpenRequest): Promise<ProjectValue> {
    if (typeof request?.path !== "string" || request.path.length === 0) {
      throw new RemoteError(
        "gateway/bad-request",
        "project/open requires a non-empty path",
        {},
      );
    }
    try {
      return projectValue(await this.ctx.workspaceRegistry.create(request.path));
    } catch (error: unknown) {
      throw new RemoteError(
        "project/open-failed",
        `cannot open project: ${errorMessage(error)}`,
        { path: request.path },
      );
    }
  }

  @Remote
  get(request: ProjectGetRequest): ProjectValue {
    if (typeof request?.projectId !== "string" || request.projectId.length === 0) {
      throw new RemoteError(
        "gateway/bad-request",
        "project/get requires projectId",
        {},
      );
    }
    return projectValue(workspaceFor(this.ctx, request.projectId));
  }

  @Remote
  async read(
    request: ProjectReadRequest,
    signal: AbortSignal,
  ): Promise<{ readonly content: string }> {
    if (
      typeof request?.projectId !== "string" || request.projectId.length === 0
      || typeof request.path !== "string" || request.path.length === 0
      || request.path.includes("\0")
    ) {
      throw new RemoteError(
        "gateway/bad-request",
        "project/read requires projectId and a non-empty relative path",
        {},
      );
    }
    const { projectId, path } = request;
    const canonical = await existingProjectFile(
      this.ctx,
      projectId,
      path,
      "project/read-failed",
    );

    try {
      signal.throwIfAborted();
      const flags = constants.O_RDONLY
        | (constants.O_NONBLOCK ?? 0)
        | (constants.O_NOFOLLOW ?? 0);
      const handle = await open(canonical, flags);
      try {
        if (!(await handle.stat()).isFile()) {
          throw new Error("project path is not a regular file");
        }
        const bytes = Buffer.alloc(MAX_TEXT_BYTES + 1);
        let offset = 0;
        while (offset < bytes.byteLength) {
          signal.throwIfAborted();
          const { bytesRead } = await handle.read({
            buffer: bytes,
            offset,
            length: bytes.byteLength - offset,
            position: offset,
          });
          if (bytesRead === 0) break;
          offset += bytesRead;
        }
        return { content: decodeText(bytes.subarray(0, offset)) };
      } finally {
        await handle.close();
      }
    } catch (error: unknown) {
      throw new RemoteError(
        "project/read-failed",
        `cannot read project file: ${errorMessage(error)}`,
        { projectId, path },
      );
    }
  }

  @Remote
  async write(request: ProjectWriteRequest, signal: AbortSignal): Promise<void> {
    if (
      typeof request?.projectId !== "string" || request.projectId.length === 0
      || typeof request.path !== "string" || request.path.length === 0
      || request.path.includes("\0") || typeof request.content !== "string"
    ) {
      throw new RemoteError(
        "gateway/bad-request",
        "project/write requires projectId, a non-empty relative path and string content",
        {},
      );
    }
    const { projectId, path } = request;
    const canonical = await existingProjectFile(
      this.ctx,
      projectId,
      path,
      "project/write-failed",
    );
    if (Buffer.byteLength(request.content, "utf8") > MAX_TEXT_BYTES) {
      throw new RemoteError(
        "project/write-failed",
        `text exceeds the ${MAX_TEXT_BYTES}-byte limit`,
        { projectId, path },
      );
    }

    const content = Buffer.from(request.content, "utf8");
    try {
      signal.throwIfAborted();
      const flags = constants.O_WRONLY
        | (constants.O_NONBLOCK ?? 0)
        | (constants.O_NOFOLLOW ?? 0);
      const target = await open(canonical, flags);
      let mode: number;
      try {
        const metadata = await target.stat();
        if (!metadata.isFile()) {
          throw new Error("project path is not a regular file");
        }
        mode = metadata.mode;
      } finally {
        await target.close();
      }

      // A temporary file keeps the old contents intact until the new contents
      // are complete. The rename and any later cancellation expose either version.
      const temporaryDirectory = await mkdtemp(join(dirname(canonical), ".metis-write-"));
      try {
        const temporary = join(temporaryDirectory, "content");
        const handle = await open(
          temporary,
          constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL,
          mode,
        );
        try {
          signal.throwIfAborted();
          await handle.writeFile(content);
          await handle.sync();
        } finally {
          await handle.close();
        }
        await chmod(temporary, mode);
        signal.throwIfAborted();
        await rename(temporary, canonical);
      } finally {
        // Cleanup failure must not misreport a rename that already committed.
        await rm(temporaryDirectory, { recursive: true, force: true }).catch(() => {});
      }
    } catch (error: unknown) {
      throw new RemoteError(
        "project/write-failed",
        `cannot write project file: ${errorMessage(error)}`,
        { projectId, path },
      );
    }
  }
}
