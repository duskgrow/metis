import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

// These licenses were reviewed for the JavaScript dependency graph. Keep this
// separate from deny.toml so the JavaScript review does not widen Rust policy.
const additionalAllow = new Set([
  "BSD-2-Clause",
  "BSD-3-Clause",
  "BlueOak-1.0.0",
  "Python-2.0",
  "Unlicense",
]);

// Copyleft stays package-scoped. Platform packages are matched as families so
// each host can validate its installed graph without requiring other hosts' files.
const packageExceptions = [
  {
    license: "MPL-2.0",
    packages: ["@deepseek-ai/libreoffice-kit"],
    packagePrefix: "@deepseek-ai/libreoffice-kit-",
  },
  {
    license: "LGPL-3.0-or-later",
    packagePrefixes: ["@img/sharp-libvips-", "@img/sharp-win32-"],
  },
];

const denyToml = readFileSync(new URL("../deny.toml", import.meta.url), "utf8");
const licenseSection = denyToml.slice(denyToml.indexOf("[licenses]"));
const allowBlock = licenseSection.match(/\ballow\s*=\s*\[([\s\S]*?)\]/);
if (!allowBlock) throw new Error("cannot read [licenses].allow from deny.toml");
const baseAllow = new Set([...allowBlock[1].matchAll(/"([^"]+)"/g)].map(match => match[1]));

export function checkLicenseReport(report, { checkStale = true } = {}) {
  const errors = [];
  const usedAdditional = new Set();
  const usedExceptions = new Set();
  let packageCount = 0;

  function globallyAllowed(license) {
    if (baseAllow.has(license)) return true;
    if (additionalAllow.has(license)) {
      usedAdditional.add(license);
      return true;
    }
    return false;
  }

  function exceptionAllowed(license, name) {
    const index = packageExceptions.findIndex(rule =>
      rule.license === license && (
        rule.packages?.includes(name)
        || (rule.packagePrefix && name.startsWith(rule.packagePrefix))
        || rule.packagePrefixes?.some(prefix => name.startsWith(prefix))
      )
    );
    if (index < 0) return false;
    usedExceptions.add(index);
    return true;
  }

  function expressionAllowed(expression, name) {
    let normalized = expression.trim();
    if (/[()]/.test(normalized)) {
      // Accept one pair around the whole expression (the shape used by the
      // current graph). Reject other grouping instead of changing its meaning.
      if (!(normalized.startsWith("(") && normalized.endsWith(")"))) return false;
      normalized = normalized.slice(1, -1);
      if (/[()]/.test(normalized)) return false;
    }
    return normalized.split(/\s+OR\s+/).some(orBranch =>
      orBranch.split(/\s+AND\s+/).every(part => globallyAllowed(part.trim()) || exceptionAllowed(part.trim(), name))
    );
  }

  for (const [license, packages] of Object.entries(report)) {
    for (const pkg of packages) {
      packageCount += 1;
      if (!expressionAllowed(license, pkg.name)) {
        errors.push(`${pkg.name}@${pkg.versions.join(",")}: ${license}`);
      }
    }
  }

  if (checkStale) {
    for (const license of additionalAllow) {
      if (!usedAdditional.has(license)) errors.push(`stale JavaScript license allowance: ${license}`);
    }
    for (const [index, rule] of packageExceptions.entries()) {
      if (!usedExceptions.has(index)) {
        const packages = rule.packages ?? rule.packagePrefix ?? rule.packagePrefixes;
        errors.push(`stale JavaScript package exception: ${rule.license} (${packages})`);
      }
    }
  }
  return { errors, packageCount };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const report = JSON.parse(readFileSync(0, "utf8"));
  const { errors, packageCount } = checkLicenseReport(report);
  if (errors.length > 0) {
    console.error(`JavaScript license policy failed:\n${errors.map(error => `- ${error}`).join("\n")}`);
    process.exitCode = 1;
  } else {
    console.log(`JavaScript license policy: ${packageCount} installed packages accepted`);
  }
}
