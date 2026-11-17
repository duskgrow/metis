import assert from "node:assert/strict";
import test from "node:test";

import { checkLicenseReport } from "./check-js-licenses.mjs";

function pkg(name) {
  return { name, versions: ["1.0.0"] };
}

function platformReport() {
  return {
    MIT: [pkg("example")],
    "BSD-2-Clause": [pkg("bsd-two")],
    "BSD-3-Clause": [pkg("bsd-three")],
    "BlueOak-1.0.0": [pkg("blue-oak")],
    "Python-2.0": [pkg("python-license")],
    Unlicense: [pkg("unlicense")],
    "MPL-2.0": [pkg("@deepseek-ai/libreoffice-kit"), pkg("@deepseek-ai/libreoffice-kit-wasm")],
    "LGPL-3.0-or-later": [pkg("@img/sharp-libvips-linux-x64")],
  };
}

test("the Linux platform graph is accepted", () => {
  assert.deepEqual(checkLicenseReport(platformReport()).errors, []);
});

test("the macOS native package families are accepted", () => {
  const report = platformReport();
  report["MPL-2.0"] = [pkg("@deepseek-ai/libreoffice-kit-darwin-arm64")];
  report["LGPL-3.0-or-later"] = [pkg("@img/sharp-libvips-darwin-arm64")];
  assert.deepEqual(checkLicenseReport(report).errors, []);
});

test("Windows Sharp's Apache AND LGPL expression is accepted", () => {
  const report = platformReport();
  report["MPL-2.0"] = [pkg("@deepseek-ai/libreoffice-kit-win32-x64")];
  delete report["LGPL-3.0-or-later"];
  report["Apache-2.0 AND LGPL-3.0-or-later"] = [pkg("@img/sharp-win32-x64")];
  assert.deepEqual(checkLicenseReport(report).errors, []);
});

test("an unknown license is rejected", () => {
  const report = platformReport();
  report["GPL-3.0-only"] = [pkg("unreviewed")];
  assert.match(
    checkLicenseReport(report).errors.join("\n"),
    /unreviewed@1\.0\.0: GPL-3\.0-only/,
  );
});

test("an MIT OR expression chooses the reviewed branch", () => {
  const report = platformReport();
  report["(MIT OR CC0-1.0)"] = [pkg("type-fest")];
  assert.deepEqual(checkLicenseReport(report).errors, []);
});

test("parentheses cannot move an unreviewed license into an OR branch", () => {
  const report = platformReport();
  const expression = "(MIT OR Apache-2.0) AND GPL-3.0-only";
  report[expression] = [pkg("unreviewed")];
  assert.match(
    checkLicenseReport(report).errors.join("\n"),
    /unreviewed@1\.0\.0: \(MIT OR Apache-2\.0\) AND GPL-3\.0-only/,
  );
});

test("stale allowances and exceptions fail closed", () => {
  const { errors } = checkLicenseReport({ MIT: [pkg("example")] });
  assert.match(errors.join("\n"), /stale JavaScript license allowance/);
  assert.match(errors.join("\n"), /stale JavaScript package exception/);
});
