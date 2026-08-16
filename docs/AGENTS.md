# AGENTS.md — The documentation standard

This file defines document structure, Markdown tiers, writing rules, and Agent Notes for this repository (a fork of `get-convex/convex-backend` with custom features on top: WASM multi-language functions, security hardening, subscription dedup, and hot-path performance work). The structure follows the documentation system shown by [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness): contracts live in docs, decisions live in Agent Notes, and every fact has one home. Use the [dsh-doc-standards](../.agents/skills/dsh-doc-standards/SKILL.md) skill for placement and validation.

## Document structure

These rules apply to human-facing documentation; Agent Notes (`.agents/notes/`) remain outside their scope. A document's subject and tree position fix its scope: describe its own subject at appropriate detail and direct children only by purpose, responsibility, and high-level behavior; link to the owning descendant for lower-level detail. A reference may be exhaustive only about its own subject.

Classify every document as a tutorial or reference. Tutorials follow an ordered path to an outcome and introduce only what each step needs. References define a lookup scope and current behavior without a teaching sequence.

Author in this order: locate the document in the tree; set its permitted detail; choose tutorial or reference; relocate descendant-owned detail; replace lower-level explanations with links to their owners.

## The tier taxonomy: one home per fact

Each fact has one home: the tier whose job it is; elsewhere, link there.

| Tier | Job | Does NOT belong there |
|---|---|---|
| Root `AGENTS.md` | Standing orders: rules an agent needs in context in every session, one to three lines each, linking its home | Stories, worked examples, anything restated from a linked home |
| `docs/` reference pages | Current behavior of a custom feature: [wasm.md](wasm.md), [non-js-languages.md](non-js-languages.md), [optimization-notes.md](optimization-notes.md), [feature-requests.md](feature-requests.md) | Decision rationale (→ Agent Notes), operator env vars (→ knobs.md) |
| `self-hosted/advanced/knobs.md` | Every environment variable a self-hosted operator can set, with default and effect | Rationale (→ Agent Notes), wasm runtime details (→ wasm.md) |
| Agent Notes (`.agents/notes/`) | Active decision records: the why, what-was-given-up, and required verification; `implemented/` notes describe shipped reality in present tense | Migration plans, checklists, spec-speak ("should…") once the decision has shipped |
| Commit messages | Change stories | Anything durable the notes must survive without them |

Placement: rationale → Agent Notes; behavior → `docs/` reference pages; operator config → `knobs.md`; standing orders → root `AGENTS.md` with a link to the owning note.

## Writing rules

- **Document current state, not change history.** Avoid "previously/now/no longer", PRs, commits, and stack positions in durable prose; name the live mechanism. Put change stories in commits and Agent Notes.
- **Every non-trivial change includes at least one Agent Note in the same change.** Update the owning note or add one; only mechanical/local edits are exempt ([scope](../.agents/notes/README.md#when-to-write-one)).
- **One physical line per paragraph**: use editor soft-wrap. Code blocks, tables, and list structure keep their formatting.
- **Fenced code blocks must compile or be marked as type-equivalent**: a pasted Rust type declaration uses ` ```rust type-equiv `, so it cannot silently drift from source; a generated catalog block uses ` ```txt generated ` and is never hand-edited.
- **Comments and JSDoc state complete contracts, not reasoning transcripts.** Preserve behavior, failure, timing, ownership, modality, exceptions, consequences, and non-obvious orientation; delete narration, test walkthroughs, review analysis, and code restatement.
- Write directly: name actors and facts. Name the exact check, type, API, operation, or behavior instead of metaphorical "gate", "vocabulary", or "surface".
- **Cross-reference with machine-checkable links, never free prose.** Link repository references with relative Markdown paths, never bare filenames. `verify-md-links` rejects missing targets and dead `#fragment` anchors.

## Wordcount budgets

Standing docs get ceilings so relocation decisions happen at writing time. Targets: this file ≤ 1,000 words; `docs/wasm.md` ≤ 1,800; `docs/optimization-notes.md` ≤ 2,000; `.agents/notes/README.md` ≤ 500. Reference pages and Agent Notes are unbudgeted: length is legitimate there when every row is a fact. When a budget is exceeded: 1. Relocate content that belongs in another tier; leave a one-line link. 2. Condense content that belongs here but can be shorter. 3. Raise the ceiling only when the words need the space.

## The slop checklist

Hunt these in any document:

- The same rule stated in more than one home. Grep a distinctive phrase; keep one home and link the rest.
- Narrated history or war stories: "previously", "now", "no longer", "renamed", PRs, or commits in durable prose. State the current fact; link an Agent Note when needed.
- Implementation-status annotations ("implemented!", "future: …"). Status rots; the notes lifecycle folders carry it.
- Hand-restated JSDoc, catalogs, or inventories of tests, packages, and status when source is authoritative.
- Reasoning transcripts: step-by-step implementation narration, proof of obvious branches, test walkthroughs, or rejected local alternatives. Keep the resulting contract or durable rationale; delete the path used to derive it.
- Rationale repeated beside sibling methods instead of once at the owning capability.
- Paragraph walls: one paragraph carrying several rules and parenthetical asides.
- Emphasis inflation: bold, CAPS, or "critically" everywhere means nothing stands out.
- Spec-speak in `implemented/` Agent Notes: "should", migration plans, acceptance checklists. An implemented note describes what is.

## Omitted dsh mechanisms

This fork adopts the dsh structure but not its machinery: there is no `.i18n.yaml` bilingual pairing (English-only repo), no VitePress website projection, and no mechanical gates yet. Conventions here are enforced by review until a gate exists; that status is itself the one home for this fact.
