---
name: adding-dependencies
description: Required procedure before adding or upgrading any third-party dependency. cargo-deny and pnpm check licenses and advisories mechanically; this skill is the judgment layer on top — necessity, maintenance health, alternatives, feature minimization — and the ASK-first discipline from AGENTS.md.
---

# Adding dependencies

A dependency is long-term inventory: every one is a supply-chain exposure, a
compile-time cost, and an upgrade obligation. AGENTS.md says ASK first — this
skill is what to prepare before asking.

## 1. Judgment (the part machines can't do)

- **Necessity**: can the stdlib or an already-present dependency do this?
  Write the one-sentence answer — "I need X because Y" — into the PR
  description.
- **Maintenance health**: recent commits/releases, issue response, bus factor,
  ownership reputation. An abandoned micro-crate is worse than ten lines of
  own code; a well-maintained one is better than a hundred.
- **Alternatives**: name the runner-up and why it lost. If none was
  considered, the evaluation isn't done.
- **License**: must pass the applicable Cargo/pnpm license policy. A new
  license → stop, surface it to the human, and let them decide before any code
  changes.
- **Fit**: for Rust, check the workspace MSRV and minimize features; for
  JavaScript, check the pinned Node runtime and review lifecycle scripts instead
  of allowing them wholesale.

## 2. Mechanical part (SSOT discipline)

For Rust:

1. Declare the version **only** in the root `Cargo.toml`'s
   `[workspace.dependencies]`. Member crates inherit with
   `dep.workspace = true` and may only add `features` / `optional` — never
   restate a version.
2. Regenerate `Cargo.lock` with cargo; never hand-edit it.

For JavaScript:

1. Keep shared development-tool versions in the root `package.json`; package
   manifests declare their own runtime dependencies. Use pnpm to regenerate
   `pnpm-lock.yaml`; never hand-edit it.
2. Record every lifecycle-script decision individually in `allowBuilds` in
   `pnpm-workspace.yaml`. Use `false` for reviewed scripts that the current
   runtime does not need; enable a script only after establishing the need.

For either ecosystem, run `just deny` (advisories / licenses / sources) and
`just ci`. Dependabot opens grouped weekly upgrade PRs afterwards; no manual
version gardening.

## 3. Temporary exceptions carry a staleness fuse

When a dependency forces an advisory/license/source exception, the exception is temporary inventory and must be
self-cleaning:

- **Per-package, never global**: use the applicable ecosystem's package-scoped
  exception mechanism. Widening a global `allow` list silently admits every
  future package under that license — the gate must keep biting new arrivals.
- **Comment the removal trigger** on the entry: which dependency brings it
  in, and what change removes it.
- **The fuse is the gate**: `just deny` denies each exception kind's
  staleness lint (the recipe is the flag-list SSOT), so an entry whose
  target has left the tree fails CI. The dependency-update PR that clears
  the condition is the one forced to delete or re-pin the entry — nobody
  has to remember.

The same shape applies to any temporary workaround, dependency-related or
not: ship it with a machine-checkable clearing condition wired into
`just ci`; if no machine check exists, attach it to a tracked milestone or
issue instead of a lone code comment.

## 4. Ask

Per AGENTS.md, adding a third-party dependency requires human confirmation.
Present the section-1 judgment summary (necessity / health / alternatives /
license / features) and wait for the answer before committing the change.
