# personal/

This layer is private. It holds notes you want the agent to keep but never
share — and dreamd enforces that:

> The `personal/` layer is excluded from LLM calls unless `--share-personal` is passed.

Concretely (from the dreamd spec):

- Implementations **MUST NOT** include `personal/` contents in any LLM
  invocation — local or remote — without explicit per-call user consent.
- The dream cycle **MUST NOT** distill `personal/` into `semantic/`.

That is why this folder carries **no lessons** — only this README. Nothing here
is ever consolidated into `semantic/LESSONS.md`. Anything you drop in `personal/`
stays in `personal/`.
