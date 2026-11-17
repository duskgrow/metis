import assert from "node:assert/strict";
import test from "node:test";

import { apply, loadMarker, name } from "../lib/index.js";

test("the host package exports a Cordis plugin", () => {
  assert.equal(name, "metis-host");
  assert.equal(typeof apply, "function");
  assert.equal(loadMarker, "[metis] host plugin loaded");
});
