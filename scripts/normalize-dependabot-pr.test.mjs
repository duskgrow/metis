import assert from "node:assert/strict";
import test from "node:test";
import { bodyIsOriginal, normalize, NORMALIZED_BODY } from "./normalize-dependabot-pr.mjs";

const input = {
  repository: "duskgrow/metis",
  number: "7",
  originalBody: "Original Dependabot body",
};

test("only an untouched opening body is replaced", () => {
  assert.equal(bodyIsOriginal(input.originalBody, input.originalBody), true);
  assert.equal(bodyIsOriginal(input.originalBody, "Maintainer edit"), false);
  assert.equal(bodyIsOriginal(undefined, undefined), true);
});

test("normalization edits the body and leaves one validation note", () => {
  const calls = [];
  const changed = normalize(input, (args) => {
    calls.push(args);
    return JSON.stringify({ body: input.originalBody });
  });

  assert.equal(changed, true);
  assert.equal(calls.length, 3);
  assert.equal(calls[1].at(-1), `body=${NORMALIZED_BODY}`);
  assert.match(calls[2].at(-1), /close and reopen/);
});

test("a concurrent or previous body edit is preserved", () => {
  const calls = [];
  const changed = normalize(input, (args) => {
    calls.push(args);
    return JSON.stringify({ body: NORMALIZED_BODY });
  });

  assert.equal(changed, false);
  assert.deepEqual(calls, [["api", "repos/duskgrow/metis/pulls/7"]]);
});

test("invalid PR coordinates fail before calling GitHub", () => {
  assert.throws(
    () => normalize({ ...input, number: "not-a-number" }, () => "{}"),
    /valid GitHub repository and pull request number/,
  );
});
