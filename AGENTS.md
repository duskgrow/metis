# AGENTS.md

<!-- Freshness: Commands tracks the justfile; NEVER tracks the generated-file list and .github/; Skills tracks .agents/skills/. Last reviewed: 2026-11 -->

## Commands (just is the only entry point; never bypass the quality gates)

- Core quality gate: `just ci` (run before every commit; CI adds platform and hosting checks, and the template repository additionally runs bootstrap smoke)
- Format / static checks: `just fmt` / `just lint` (rustfmt + dprint for Markdown/TOML/TS/JS; TypeScript typecheck; actionlint on workflow YAML)
- All tests: `just test`; single test: `cargo nextest run -p metis <name>`
- Iterate with the narrowest loop first (`cargo check -p <crate>`, scoped nextest); finish with `just ci`
- Snapshot updates: `just snapshot-review` (approve each diff by hand; never bulk-accept)
- New crate: `just new-crate <name>`
- Dependency policy: `just deny`; docs build: `just doc`; release-artifact drift check: `just dist-check`; agent-doc smoke check: `just agent-check`
- Toolchain bump: `just toolchain-bump` (also run weekly by CI, which opens a validated PR)

## Environment

- Toolchains come from flake.nix: `direnv allow` or `nix develop`. Version SSOTs: `rust-toolchain.toml` for Rust, `.node-version` for Node.js, and root `package.json` `packageManager` for pnpm; no other file may restate them.
- The agent terminal spawns a bare shell without the direnv hook, so `.envrc` never loads and `cargo`/`just` resolve to the user profile instead of flake.nix. Prefix such commands with `direnv exec .` when the flake toolchain is needed (e.g. `direnv exec . just ci`); this includes `git commit`, whose prek hooks run `just fmt` and `just lint`.
- `just deny` needs network access for the RustSec and npm advisory checks; when offline, skip that one item and run the rest.
- Native Windows development goes through WSL2 (Nix has no native Windows support); CI's windows job consumes the same Rust/Node pins and Cargo/pnpm lockfiles.

## Skills (procedures live in .agents/skills/; constraints stay in this file)

- `pr-preflight` — the judgment-layer review before opening/updating a PR (also the frame for reviewing others' PRs)
- `self-review` — the pre-commit pass over your working diff: mechanical sweep yourself, judgment pass delegated to a fresh subagent (author blindness is structural)
- `adding-dependencies` — required procedure before touching any third-party dependency
- `rule-maintenance` — how to update AGENTS.md and skills when rules change or mistakes repeat
- `doc-maintenance` — writing/reviewing docs: canonical ownership, reader context, Chinese-canonical translation sync, drift hunting
- `code-simplification` — behavior-preserving dedup/dead-code/abstraction cleanup, always as its own change
- `explain-diff` — maintainer-facing rich HTML walkthrough of a change + in-chat quiz, for reading large diffs
- `release-review` — the human-gate checklist for merging a release-plz Release PR

Done = `just ci` green + the `pr-preflight` checklist clean.

## NEVER

- NEVER hand-edit generated files: `Cargo.lock`, `pnpm-lock.yaml`, any generated entries in `CHANGELOG.md` (including released sections), `.github/workflows/release.yml` (generator: `dist generate`; drift check: `just dist-check`).
- NEVER write multi-line logic in a CI YAML `run:` field — sink it into the justfile; CI only calls single-line entry points.
- NEVER hand-write version numbers: the single storage point is `[workspace.package] version` in the root `Cargo.toml`; release-plz rewrites it mechanically.
- NEVER commit code that hasn't passed `just ci`; NEVER assemble ad-hoc check pipelines that bypass just.
- NEVER merge PRs, push tags, or publish releases — the agent stops at the PR gate; merging belongs to humans (branch protection is the hard boundary, not this file).
- NEVER add agent-attribution footers (`Co-Authored-By`, "Generated with …") to commits or PRs.
- NEVER force-push or rewrite shared history unless the human explicitly asks.
- ASK before touching: new third-party dependencies (see the `adding-dependencies` skill), anything under `.github/`, branch protection and crates.io Trusted Publishing settings.

## Style (the part lints can't enforce)

- Time / randomness / IO are always injected as constructor parameters; no hidden `now()` / `rand()` / global state (test determinism depends on this seam).
- When the maintainer asks what a piece of code means or why it exists, the explanation lands as a brief comment at that spot in the same change — chat answers rot. Comments explain current intent, constraints and tradeoffs without restating mechanism, narrating change history, or listing removed alternatives.
- Error split: library crates use `thiserror` enums (callers branch on failure modes); the binary's top level reports with `miette` (three-part `code` + `help`); functions that propagate errors don't log — log once at the handling site.
- Commit messages: modified Conventional Commits — `type(scope): subject`, lowercase type/scope, pure-ASCII English subject, header ≤ 100 chars, blank line before body/footer (`fix`→PATCH, `feat`→MINOR, `!`→MAJOR). Enforced at the commit-msg hook and by the PR CI check (SSOT: the `check-commit` subcommand of `crates/xtask`); merge strategy is squash-only with the PR title+body landing as the commit message — the PR title must obey the same rules, and the body is landable as a whole (CI rejects HTML comments, task lists, bare `---` lines, and prose paragraphs over 7 lines — split or use lists).
- Product documentation is Chinese-first: documents under `docs/` are written directly in Chinese. For the external-facing README and CONTRIBUTING portals, `*.zh-CN.md` is canonical and the English counterpart is a translation — update the Chinese version first. Code, commit messages, and agent operational instructions remain English.
- Product documentation is for users and developers, not a specification: lead with distinctive user outcomes, include only what code cannot express, and describe the current state without obvious feature inventories, defensive repetition, change history, or negative restatements of removed features (review procedure: the `doc-maintenance` skill).
- Docs co-evolve in the same PR as the behavior change. A rule you needed but couldn't find is a missing rule: add it via the `rule-maintenance` skill in that PR, instead of re-deriving it next session.
