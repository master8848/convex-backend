---
name: dsh-doc-standards
description: Apply the repository documentation standard when writing or editing any documentation. Use for docs/AGENTS.md compliance, doc placement ("one home per fact"), Agent Note creation or updates, worklog conversion, link checking, and slop removal. Do not use for code changes without docs.
---

# dsh-doc-standards

Guidance, not a script. The contract lives in [docs/AGENTS.md](../../../docs/AGENTS.md) and [.agents/notes/README.md](../../notes/README.md); read both before editing any documentation.

## Sources of truth

- `docs/AGENTS.md` — tier taxonomy, writing rules, word budgets, slop checklist.
- `.agents/notes/README.md` — Agent Note format, classification, lifecycle.
- Existing Agent Notes under `.agents/notes/implemented/` — precedent for uniform note format.

## Workflow

1. **Locate the fact's home first.** One home per fact: rationale → Agent Notes; current behavior → `docs/` reference pages; operator env vars → `self-hosted/advanced/knobs.md`; standing rules → `docs/AGENTS.md`. Elsewhere, link there.
2. **Classify** the document as reference or tutorial; these docs are references (lookup scope, current behavior).
3. **Apply the writing rules**: current state, not history; one physical line per paragraph; concrete prose (exact crates, files, env vars, flags — no metaphors); no reasoning transcripts or worklog narration.
4. **Non-trivial changes carry an Agent Note** in the same change; a worklog converts into one (`.agents/notes/implemented/{class}/YYYY-MM-DD-slug.md`, uniform format, then delete the worklog).
5. **Audit the slop checklist** from `docs/AGENTS.md`: duplicated facts, narrated history, status annotations, hand-restated source, emphasis inflation, paragraph walls.
6. **Verify mechanically**: every relative link resolves (target exists, `#fragment` matches a heading slug), word budgets hold, note header is exactly `# Agent Note: <title>` / blank / `Status: implemented`.

## Budgets

`docs/AGENTS.md` ≤ 1,000 words; `docs/wasm.md` ≤ 1,800; `docs/optimization-notes.md` ≤ 2,000; `.agents/notes/README.md` ≤ 500. Over budget: relocate → condense → raise with justification.
