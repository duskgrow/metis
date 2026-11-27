import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const NORMALIZED_BODY =
  "Update dependencies through Dependabot. Use the PR diff as the authoritative record of the resolved versions.";

const REVIEW_COMMENT =
  "Dependabot's body was normalized for squash merging. GitHub-token edits do not retrigger workflows. Review the title, then close and reopen this PR (or make the final title edit) to validate the final commit message.";

export function bodyIsOriginal(original, current) {
  return (current ?? "") === (original ?? "");
}

export function normalize({ repository, number, originalBody }, run) {
  if (!/^[^/]+\/[^/]+$/.test(repository ?? "") || !/^[1-9]\d*$/.test(number ?? "")) {
    throw new Error("A valid GitHub repository and pull request number are required");
  }

  const endpoint = `repos/${repository}/pulls/${number}`;
  const current = JSON.parse(run(["api", endpoint]));
  if (!bodyIsOriginal(originalBody, current.body)) return false;

  run(["api", "--method", "PATCH", endpoint, "-f", `body=${NORMALIZED_BODY}`]);
  run([
    "api",
    "--method",
    "POST",
    `repos/${repository}/issues/${number}/comments`,
    "-f",
    `body=${REVIEW_COMMENT}`,
  ]);
  return true;
}

function runGh(args) {
  return execFileSync("gh", args, {
    encoding: "utf8",
    maxBuffer: 4 * 1024 * 1024,
  });
}

function main() {
  if (!process.env.GH_TOKEN) throw new Error("GH_TOKEN is required");
  if (!process.env.GITHUB_EVENT_PATH) throw new Error("GITHUB_EVENT_PATH is required");
  const event = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, "utf8"));
  const changed = normalize(
    {
      repository: process.env.GITHUB_REPOSITORY,
      number: String(event.pull_request.number),
      originalBody: event.pull_request.body,
    },
    runGh,
  );
  console.log(changed ? "Normalized the Dependabot PR body" : "PR body changed after opening; left it untouched");
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main();
}
