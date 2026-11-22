import { apply as restartApply } from "./restart-fixture.js";

export { inject } from "./restart-fixture.js";

export function apply(ctx) {
  process.stdin.removeAllListeners("data");
  process.stdin.removeAllListeners("end");
  restartApply(ctx);
}
