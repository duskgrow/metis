---
name: doc-maintenance
description: Use when writing, updating, or reviewing documentation — README, CONTRIBUTING, docs/, AGENTS.md prose, and translated portals. Encodes this repo's documentation discipline — canonical ownership, standalone reader context, Chinese-canonical translation sync, same-PR co-evolution with code, and drift hunting.
---

# Documentation maintenance

Every hand-written document needs a clear reader and one canonical ownership
role. Prose without either becomes a drift source.

## The documentation filter (apply to every paragraph)

1. Can code or configuration state this fact? → delete the prose, or make it
   generated or referenced. Never hand-copy version numbers, command lists,
   help text, or parameter tables.
2. Does it answer _why_? → write the rationale once in an architecture decision
   record under `docs/decisions/` or in a dedicated explanation section.
3. Does it define a current component boundary, state-ownership rule, delivery
   promise, or dated research finding? → place it in the canonical document for
   that role and link to it instead of copying the rule elsewhere.
4. Is it an entry point? → write it as an executable command that continuous
   integration or a doctest can keep honest.

If none applies, do not write the paragraph.

## Placement

- `README.md` — portal: what and why in one line, installation, execution, and
  links. No parameter tables or implementation details.
- `CONTRIBUTING.md` — how-to for the development process.
- `docs/architecture.md` — current component boundaries and state ownership.
- `docs/product-scope.md` — current product scope and delivery order.
- `docs/decisions/` — architecture decision records; one decision per file,
  with rationale recorded only there.
- `docs/research/` — dated evidence about external systems, including the exact
  source version and the limits of verification.
- Application programming interface reference — rustdoc comments next to the
  code; never a separate hand-maintained copy.

## Reader context and terminology

Write for a user or maintainer who did not participate in the discussion and
does not already have a mental map of the repository.

- Lead with the distinctive user outcome. Expected baseline functionality does
  not need a feature inventory or a table of its own.
- Open with what the document helps the reader understand or do. Add status or
  scope notes only when they clarify how the document should be used.
- Describe the current state, not the change process. Remove rejected features
  and obsolete alternatives instead of restating them as negatives.
- Use prose and end-to-end scenarios when they explain relationships more
  clearly than tables. Keep implementation details beside the code.
- Use the same component names throughout and explain relationships explicitly.
- Write concise explanations for users and developers, not a specification.
  State important limitations once without defensive rationale.
- Avoid abbreviations when the full term is clearer. When an abbreviation is
  useful, introduce the full term first; do not compress every concept into
  initials merely to shorten the prose.
- Prefer complete sentences with explicit subjects over telegraphic fragments.
  Brevity is valuable only after the reader can understand the paragraph.

## Language sync

Chinese is canonical for product documentation.

- Write documents under `docs/` directly in Chinese, including architecture
  decision records and research reports. They do not require English copies.
- The external-facing README and CONTRIBUTING portals have a canonical
  `*.zh-CN.md` file and an English translation. Update the Chinese version
  **first** and the English translation in the same change.
- Paired portals link to each other. The Chinese header identifies it as the
  canonical version; the English header links to that Chinese original.
- Translation drift is a bug: flag or fix it when noticed. AGENTS.md and skills
  are operational instructions, not translated product documentation.

## Co-evolution

Behavior, commands, flags, or config changed → the affected docs change in the
**same PR**. A doc-only PR is for rot fixes and translations. Never promise
documentation "in a follow-up".

## Drift hunting (run when asked to "check the docs")

- Grep the docs for commands/files that were renamed or deleted; every hit is
  a fix.
- Version numbers, counts, paths in prose: find their authoritative source; if
  none exists, delete or generate.
- AGENTS.md / skills touched → run `just agent-check`; docs build → `just doc`.
