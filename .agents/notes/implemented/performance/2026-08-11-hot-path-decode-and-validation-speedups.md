# Agent Note: Hot-path decode, validation, and SQL filter speedups

Status: implemented

## Problem

Hot paths allocated far more than necessary: JSON document reads parsed into a full `serde_json::Value` tree before converting to `ConvexValue` (roughly 2x nodes and 2x key strings); schema validation and validator parsing spent CPU on repeated work; SQL filters deep-cloned documents and re-evaluated expressions that are constant.

## Decision

Four independent speedups ship, each with no behavior change:

- **JSON → `ConvexValue` direct parse**: `value::json_deserialize` and the SQLite/Postgres/MySQL document read paths parse internal JSON directly into `ConvexValue`, skipping the intermediate `serde_json::Value` tree.
- **Validator and schema validation speedups**: validator parsing and schema validation avoid re-parse and redundant allocation on hot paths.
- **SQL filters skip deep-cloning**: filters in `crates/sql` operate without deep-cloning documents that are only read.
- **Constant-expression folding**: filters fold constant expressions so the same value is not re-evaluated per row.

## Alternatives considered

- **Reuse a scratch `serde_json::Value` across reads**: saves allocations for one stage but keeps the two-tree structure; the direct parse removes the stage entirely.
- **Cache schema validation results**: caching risks staleness when the schema changes; the speedups reduce per-validation cost instead of memoizing.

## Consequences

- The measureable speedup for each change is recorded in `docs/optimization-notes.md`, which is the one home for the per-change numbers, reasoning, and validation.
- These changes must not alter observable query or validation semantics; any future change to the parse path runs the document-read test suites as the contract.
