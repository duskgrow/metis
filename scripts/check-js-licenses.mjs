import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

// These licenses were reviewed for the JavaScript dependency graph. Keep this
// separate from deny.toml so the JavaScript review does not widen Rust policy.
const additionalAllow = new Set(["Python-2.0"]);

const denyToml = readFileSync(new URL("../deny.toml", import.meta.url), "utf8");
const licenseSection = denyToml.slice(denyToml.indexOf("[licenses]"));
const allowBlock = licenseSection.match(/\ballow\s*=\s*\[([\s\S]*?)\]/);
if (!allowBlock) throw new Error("cannot read [licenses].allow from deny.toml");
const baseAllow = new Set([...allowBlock[1].matchAll(/"([^"]+)"/g)].map(match => match[1]));

export function checkLicenseReport(report, { checkStale = true } = {}) {
  const errors = [];
  const usedAdditional = new Set();
  let packageCount = 0;

  function globallyAllowed(license) {
    if (baseAllow.has(license)) return true;
    if (additionalAllow.has(license)) {
      usedAdditional.add(license);
      return true;
    }
    return false;
  }

  function expressionAllowed(expression) {
    let normalized = expression.trim();
    if (/[()]/.test(normalized)) {
      // Accept one pair around the whole expression (the shape used by the
      // current graph). Reject other grouping instead of changing its meaning.
      if (!(normalized.startsWith("(") && normalized.endsWith(")"))) return false;
      normalized = normalized.slice(1, -1);
      if (/[()]/.test(normalized)) return false;
    }
    return normalized.split(/\s+OR\s+/).some(orBranch =>
      orBranch.split(/\s+AND\s+/).every(part => globallyAllowed(part.trim()))
    );
  }

  for (const [license, packages] of Object.entries(report)) {
    for (const pkg of packages) {
      packageCount += 1;
      if (!expressionAllowed(license)) {
        errors.push(`${pkg.name}@${pkg.versions.join(",")}: ${license}`);
      }
    }
  }

  if (checkStale) {
    for (const license of additionalAllow) {
      if (!usedAdditional.has(license)) errors.push(`stale JavaScript license allowance: ${license}`);
    }
  }
  return { errors, packageCount };
}

export function parseLicenseReport(input) {
  // PowerShell pipelines prefix a UTF-8 BOM on Windows; JSON.parse rejects it.
  return JSON.parse(input.replace(/^\uFEFF/, ""));
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const report = parseLicenseReport(readFileSync(0, "utf8"));
  const { errors, packageCount } = checkLicenseReport(report);
  if (errors.length > 0) {
    console.error(`JavaScript license policy failed:\n${errors.map(error => `- ${error}`).join("\n")}`);
    process.exitCode = 1;
  } else {
    console.log(`JavaScript license policy: ${packageCount} installed packages accepted`);
  }
}
