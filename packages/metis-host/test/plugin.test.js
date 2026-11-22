import assert from "node:assert/strict";
import test from "node:test";

import { apply, loadMarker, name } from "../lib/index.js";
import { projectValue } from "../lib/project.js";

test("the host package exports a Cordis plugin", () => {
  assert.equal(name, "metis-host");
  assert.equal(typeof apply, "function");
  assert.equal(loadMarker, "[metis] host plugin loaded");
});

test("project metadata bounds its recent Session summary", () => {
  const sessionIds = Array.from({ length: 101 }, (_, index) => `session-${index}`);
  const project = projectValue({
    id: "project-1",
    path: "/project",
    title: "Project",
    sessionIds,
  });
  assert.equal(project.recentSessionIds.length, 100);
  assert.deepEqual(project.recentSessionIds, sessionIds.slice(0, 100));
  assert.equal(project.hasMoreSessions, true);
});
