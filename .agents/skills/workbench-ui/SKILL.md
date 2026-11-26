---
name: workbench-ui
description: "Use when designing, implementing, or reviewing Metis workbench UI/UX — layouts, components, themes, tokens, actions, interactions, states, or product-facing copy. Applies a proportional, lightweight process: local changes need no design brief; new reusable or cross-surface semantics and interaction patterns need at most a one-page brief. Existing ADR and release rules govern public contracts. Not for headless-only work or isolated GPUI integration smoke tests without product UI."
---

# Workbench UI

Use this procedure from the start of a UI task and again at review. The goal is
consistent daily decisions without turning every small change into a design
project.

Canonical product rules live in
[Workbench experience](../../../docs/workbench-experience.md). Native verification
and visual-evidence boundaries live in the
[Workbench component contracts](../../../docs/workbench-component-contracts.md).
Dated evidence lives in the [UI/UX research](../../../docs/research/workbench-uiux.md);
do not copy the research into new documents. GPUI dependency boundaries remain
governed by ADR-0006.

## 1. Classify once (use the highest applicable level)

| Level           | Examples                                                                                                                                                                                     | Required process                                                                                                                                                                 |
| --------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Local**       | Fix an existing pattern; wire one action; add a one-off component or local state; add an instance of an existing semantic; adjust a style using existing tokens                              | No separate design brief. Record the user task, existing pattern, affected states, and validation in the normal PR.                                                              |
| **New pattern** | Introduce or change reusable/cross-surface user semantics or interaction contracts: a shared component/pattern, navigation/layout, cross-surface action/state semantics, or a theme contract | Write at most a one-page brief before final implementation. Get one experience-focused and one engineering-focused review; the same maintainer may perform both in a small team. |

The classification question is: **does this introduce or change reusable or
cross-surface user semantics or an interaction contract?** If not, it is Local.
Existing ADR, semver, and release rules govern public contracts independently;
this procedure does not add another tier for them.

Headless-only and isolated component integration tests stay outside this
process. If a smoke example becomes the basis for a product pattern, classify
it at that point.

## 2. Before implementation

For **Local**, identify the existing pattern and only the states affected by the
fix. No meeting, standalone document, or full-page redesign is required.

For **New pattern**, keep the brief to at most one page (in the issue, PR, or
linked design artifact). Answer only applicable prompts; the page is a limit,
not a target:

1. User task and the problem with the current experience.
2. Representative content and the states that matter, including failure and
   recovery where applicable.
3. Existing components/patterns considered and why they are insufficient.
4. Proposed behavior, keyboard/focus path, and asynchronous-update behavior.
5. Semantic tokens or the reason a new token is necessary.
6. Accessibility, theme/density, and long-content pressure checks.
7. Validation and, for a temporary exception, its scope and removal condition.

A prototype is optional: choose the cheapest artifact that answers the open
question. Do not require high-fidelity mockups when a state flow or code probe
is sufficient.

Internal component API changes update their callers and tests; they do not
automatically require an ADR. Use the existing ADR-necessity rule in
`pr-preflight` for architectural or public-contract decisions.

## 3. During implementation

- Start from the end-to-end task and representative content, not an isolated
  gallery of ideal states.
- Reuse existing components, actions, regions, and semantic tokens. Do not
  introduce a shared abstraction for a single-use case.
- Cover affected states, not a Cartesian product of every page, theme, DPI,
  window size, and input mode.
- Keep visual decisions in semantic tokens. Record any temporary exception in
  the PR with its scope and removal condition.
- Preserve context, focus, text selection, and scroll anchors across streaming
  updates and theme/density changes.
- Use Component/Base through `gpui-kit`; do not add direct GPUI dependencies.
- Treat approvals and untrusted Markdown as security boundaries, not only as
  visual components.

## 4. Review proportionally

Check every applicable item and mark the rest not applicable; do not manufacture
work to fill the list.

- [ ] The user task, host/project/session context, and next action are clear.
- [ ] Affected loading, empty, error, disabled, stale, and recovery states are
      honest and distinguishable.
- [ ] Keyboard navigation and focus entry/exit/restoration work; asynchronous
      updates do not steal focus or reading position.
- [ ] Real Chinese text, long paths/content, narrow layouts, and affected
      theme/density modes preserve content and actions.
- [ ] Status is not conveyed by color alone; relevant contrast and reduced
      motion behavior are checked.
- [ ] Approval scope and untrusted-content behavior are safe where applicable.
- [ ] Tests cover the behavior at the lowest effective layer. Visual evidence
      covers only the states affected by the change.
- [ ] Exceptions, lifecycle, documentation, and migration match the selected
      level.

**Local** needs no review beyond the normal `self-review` and `pr-preflight`.
**New pattern** gets the two focused checks described above, without requiring
a recurring meeting. Existing ADR and release rules apply independently.

The repository maintainer owns the final experience and engineering decisions.
Reviews may be delegated, but a subagent review is advice, not approval.

## 5. Finish

Run `just ci`. Co-evolve the component contract, canonical experience document,
or ADR in the same change when the corresponding boundary actually changes;
do not create another copy of existing rules.

Promote a reusable pattern to Stable only after real use has validated its
contract. Close temporary exceptions as part of the task that makes them
unnecessary. Change this procedure through `rule-maintenance` when its cost or
coverage stops matching reality.
