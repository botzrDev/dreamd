---
name: RFC (spec or design change)
about: Propose a change to SPEC.md, the on-disk layout, or a load-bearing design decision
title: "[RFC] "
labels: rfc
assignees: ''
---

## Proposal

One paragraph: what you want to change.

## Motivation

Why does the current behavior fall short? Cite a concrete use case, harness, or workflow that the change unlocks.

## Affected surface

Tick all that apply:

- [ ] `SPEC.md` (on-disk layout, schemas, salience formula, dream-cycle contract)
- [ ] HTTP API (`/api/v1/...`)
- [ ] MCP tool surface (`search_nodes`, `append_node`, ...)
- [ ] CLI (`dreamd ...`)
- [ ] File formats (`AGENT_LEARNINGS.jsonl`, `LESSONS.md`, etc.)
- [ ] Other (describe)

## Detailed design

The proposed change in enough detail that a reviewer can spot edge cases. Include schema diffs, API signatures, or pseudocode where useful.

## Backwards compatibility

Does this break existing `.agent/` directories on disk, existing harness integrations, or existing API consumers? If so, what's the migration path? (`schema_version` bump? `dreamd migrate` step?)

## Alternatives considered

What else did you look at, and why is this proposal better?

## Open questions

Anything you want reviewers to weigh in on specifically.
