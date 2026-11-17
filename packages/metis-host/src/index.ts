import type { Context } from "@deepseek-ai/cordis";

export const name = "metis-host";
export const loadMarker = "[metis] host plugin loaded";

export function apply(_ctx: Context): void {
  console.log(loadMarker);
}
