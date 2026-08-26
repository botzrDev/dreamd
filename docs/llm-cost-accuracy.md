# LLM cost estimate accuracy

How `cost_cap_usd` is enforced, how accurate the number behind it is, and what to do when prices move. The short version: the estimate is deliberately approximate and deliberately biased toward *not* calling the model.

**Canonical source:** `crates/dreamd-core/src/llm.rs` (`rates_for`, `estimate_prompt_cost`, `exceeds_cap`, `OUTPUT_RESERVE_TOKENS`)

---

## Where the estimate happens

`dream_cycle::compose_lesson_body` builds the composition prompt, prices it, and only then decides whether to call the model. Nothing is sent over the network before the decision:

1. Build the prompt (`llm::build_lesson_prompt`).
2. `llm::estimate_prompt_cost(model, &prompt)` → `Option<CostEstimate>`.
3. `None`, or `llm::exceeds_cap(&estimate, cap_usd)` → log a `cost estimate exceeds cap` `WARN` and return the deterministic exemplar body.
4. Otherwise, the normal retry + citation path.

Pricing the prompt at the call site — rather than inside the genai client — means a retried request never re-counts the same prompt, and the cap decision stays unit-testable without a backend.

---

## The formula

```text
estimated_usd = (input_tokens  / 1000) * input_per_1k
              + (OUTPUT_RESERVE_TOKENS / 1000) * output_per_1k
```

`input_tokens` is the `cl100k_base` token count of the assembled prompt. `OUTPUT_RESERVE_TOKENS` is `512` — a fixed pad standing in for the completion, whose length cannot be known before it exists.

---

## The tokenizer is `cl100k_base` for every model

Token counting uses `tiktoken-rs` with the **bundled** `cl100k_base` encoding, for **every** model — Anthropic models included. No encoding is downloaded at run time.

That is knowingly the wrong tokenizer for Claude. Anthropic segments text differently, and on prose of this shape — clustered learning events plus a short instruction block — the counts differ by roughly **±25%**. The estimate is therefore an order-of-magnitude guard, not an invoice.

**Consequence:** real spend can reach roughly **$0.13** on a cycle that estimated under a **$0.10** cap. That overshoot is accepted. The cap exists to stop a runaway cycle, not to bill to the cent.

---

## Why there is no remote token-count call

dreamd does **not** call a provider's token-counting endpoint before composing a lesson. Two reasons:

- **It is a network round trip taken to decide whether to make a network trip** — on the exact path that is supposed to be the cheap one. A cap that costs an HTTP call to evaluate cannot be evaluated on every cycle, and one that is evaluated only sometimes is not a cap.
- **It moves a failure mode into the guard.** The guard would then have its own timeout, its own retry, and its own outage — and the safe behavior when the *cost check* fails is exactly the behavior it was protecting against.

The rejected alternative was vendoring a copy of Anthropic's tokenizer. That tokenizer is not published, so a vendored copy would be reverse-engineered, unversioned, and silently wrong the moment it drifted — a worse form of the same ±25% error, with none of the honesty.

---

## The cap errs toward abort

Every ambiguity resolves in the direction of *don't spend money*:

| Situation | Result |
|---|---|
| Estimate ≤ `cost_cap_usd` | Model call proceeds |
| Estimate > `cost_cap_usd` | No model call; deterministic exemplar body |
| Model id not in `rates_for` | `estimate_prompt_cost` returns `None` → treated as over cap |
| Tokenizer fails to build | `None` → treated as over cap |
| `cost_cap_usd <= 0.0` | Always over cap — `0.0` means "never call the model", not "no limit" |

`OUTPUT_RESERVE_TOKENS = 512` is a generous pad on purpose: it is larger than any lesson this prompt produces. Under-reserving means the cap passes and the bill arrives anyway; over-reserving means a borderline cycle composes deterministically. Only the second is something an operator can fix — raise `cost_cap_usd`.

**Over-cap never fails the cycle.** It takes the same arm as a missing API key or a failed completion: `LessonBodySource::Deterministic`, a structured `WARN`, exit 0. `dreamd dream --dry` degrades identically on the preview path.

To see the number before a cycle runs:

```bash
dreamd doctor --cluster-health
# cluster_health: next_cycle_est_usd=0.0043 cap_usd=0.10 model=claude-haiku-4-5 tokens=3821 (within cap)
```

---

## Price table — snapshot 2026-08-24

`rates_for` in `crates/dreamd-core/src/llm.rs` is a **hand-maintained snapshot**, not a live lookup. Published prices live behind a provider dashboard, and a cap that silently re-prices itself over the network is a cap the operator cannot reason about.

| Model | Input per 1K tokens | Output per 1K tokens |
|---|---|---|
| `claude-haiku-4-5` (alias `claude-haiku-4.5`) | `$0.001` | `$0.005` |
| `gpt-4o-mini` | `$0.00015` | `$0.0006` |

Any other model id falls through to `None` and is treated as over cap. Aliases are spelled out rather than normalized because the model string is operator-typed config: a typo should abort, not silently price as something else.

### Refreshing the prices

The table goes stale in place, so refreshing it is a **three-part edit that belongs in one commit**:

1. The match arms in `rates_for` (`crates/dreamd-core/src/llm.rs`).
2. The table and snapshot date in **this document**.
3. The snapshot date in the `rates_for` / `ModelRates` rustdoc.

Split them and the docs start describing rates the binary no longer charges against.

---

## Binary size

The workspace `[profile.release]` sets `lto = "fat"` + `codegen-units = 1`; that fat-LTO build is what dead-strips the unused encodings and keeps NFR-2 (stripped `dreamd` ≤ 20 MB) green with `tiktoken-rs` linked in.

---

## See also

- [configuration.md](./configuration.md) — `cost_cap_usd` and the rest of the config surface
- [troubleshooting.md](./troubleshooting.md) — dream-cycle symptoms and fixes
- [../SPEC.md](../SPEC.md) — dream-cycle contract and `LESSONS.md` shape
