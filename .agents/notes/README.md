# Agent Notes

Agent Notes are decision records: the why, what-was-given-up, and required verification behind a shipped behavior. They exist so a maintainer can reconstruct a decision and its rejected alternatives without re-litigating them.

## When to write one

Every non-trivial change adds or updates at least one Agent Note in the same change. A change is non-trivial when it alters behavior, architecture, a contract shared across crates, a configuration format, a wire format, an on-disk format, tooling, or testing strategy — any decision a maintainer may reasonably revisit. Purely mechanical or local edits with no change to behavior, contracts, structure, process, or rationale are exempt.

## Directory layout

```
.agents/notes/
  README.md
  implemented/
    feature/        new user- or operator-facing capability
    bug-fix/        corrects a defect
    simplification/ removes code or surface area
    architecture/   structural decision about shipped source
    performance/    hot-path speedup with no behavior change
    process/        tooling, policy, or workflow around the code
  proposed/         same classes — decisions being considered
  rejected/         proposals that were not adopted
```

The folder is the status and the class; the file name carries the first-proposed date: `implemented/feature/2026-08-13-self-hosted-security-hardening.md`. No prose line restates the class — the path is the fact.

## Uniform format

First three lines exactly:

```markdown
# Agent Note: <title>

Status: implemented
```

`Status:` is `proposed`, `implemented`, or `rejected — <why, in one line>`, and must agree with the lifecycle folder the file sits in. No dates in the status line; the filename holds the date.

Implemented notes use this skeleton:

```markdown
# Agent Note: <title>

Status: implemented

## Problem

## Decision

## Alternatives considered

## Consequences
```

Proposed notes use `## Proposal` instead of `## Decision`, plus `## Acceptance criteria` and `## Risks`. Implemented notes describe shipped reality in present tense; `## Proposal`, `## Migration plan`, and `## Acceptance criteria` are spec-speak and do not appear there. `## Alternatives considered` is mandatory — a decision recorded without what it beat invites re-litigation.

## Lifecycle

- `proposed/` → `implemented/`: rewrite `Proposal` as `Decision`, fold acceptance criteria and risks into `Consequences`, in the same change that implements the decision.
- `implemented/` notes stay current with what actually shipped: update facts (paths, names, structure) in the same change that moves them.
- `rejected/` is the proposal frozen; the verdict lives on the `Status:` line.
- A note is never edited into a different decision: supersede it with a new note.

## Rules

- Cross-link notes and docs with relative Markdown paths, never bare filenames or note numbers.
- Keep every note's facts in one home: rationale here, current behavior in the owning `docs/` reference page, operator env vars in `self-hosted/advanced/knobs.md`.
- Duplicate notes for the same decision are forbidden; update the owning note instead.
