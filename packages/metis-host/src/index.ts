import type { Context } from "@deepseek-ai/cordis";

import { GitService } from "./git.js";
import { ProjectService } from "./project.js";

export const name = "metis-host";
export const loadMarker = "[metis] host plugin loaded";

export function apply(ctx: Context): void {
  ctx.plugin(ProjectService);
  ctx.plugin(GitService);
  console.log(loadMarker);
}
