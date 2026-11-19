import assert from "node:assert/strict";
import test from "node:test";

import { checkLicenseReport } from "./check-js-licenses.mjs";

function pkg(name) {
  return { name, versions: ["1.0.0"] };
}

function baseReport() {
  return {
    MIT: [pkg("example")],
    "Python-2.0": [pkg("python-license")],
  };
}

test("the reviewed license allowances are accepted", () => {
  assert.deepEqual(checkLicenseReport(baseReport()).errors, []);
});

test("an unknown license is rejected", () => {
  const report = baseReport();
  report["GPL-3.0-only"] = [pkg("unreviewed")];
  assert.match(
    checkLicenseReport(report).errors.join("\n"),
    /unreviewed@1\.0\.0: GPL-3\.0-only/,
  );
});

test("an MIT OR expression chooses the reviewed branch", () => {
  const report = baseReport();
  report["(MIT OR CC0-1.0)"] = [pkg("type-fest")];
  assert.deepEqual(checkLicenseReport(report).errors, []);
});

test("parentheses cannot move an unreviewed license into an OR branch", () => {
  const report = baseReport();
  const expression = "(MIT OR Apache-2.0) AND GPL-3.0-only";
  report[expression] = [pkg("unreviewed")];
  assert.match(
    checkLicenseReport(report).errors.join("\n"),
    /unreviewed@1\.0\.0: \(MIT OR Apache-2\.0\) AND GPL-3\.0-only/,
  );
});

test("stale allowances fail closed", () => {
  const { errors } = checkLicenseReport({ MIT: [pkg("example")] });
  assert.match(errors.join("\n"), /stale JavaScript license allowance/);
});
