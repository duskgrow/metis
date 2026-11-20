import { readFileSync } from "node:fs";

const expected = readFileSync(new URL("../.node-version", import.meta.url), "utf8").trim();
const actual = process.versions.node;
if (actual !== expected) {
  console.error(
    `Metis requires Node.js ${expected} for dsh, but the current executable is ${actual}.\n`
      + "Use the Nix devShell or select the version from .node-version before running just.",
  );
  process.exitCode = 1;
}
