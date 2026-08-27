//! AILAB-204 / DR-304 — LLM client wrapper with deterministic auto-fallback.
//!
//! The dream cycle composes `LESSONS.md` bodies through an [`LlmBackend`] when
//! credentials are present, and silently runs the deterministic exemplar-copy
//! path otherwise. **The cycle never fails because the model coughed**: every
//! error path here is a fallback signal, not a cycle error. Callers translate
//! `Err` / empty text into [`crate::consolidation::LessonBodySource::Deterministic`].
//!
//! Since AILAB-201 this module also owns the composition prompt itself:
//! [`crate::llm::LESSON_PROMPT`] and the [`crate::llm::VERSIONED_PROMPT_ID`]
//! stamped on every LLM-composed `LESSONS.md`. (Paths are fully qualified
//! because `lib.rs` carries an outer doc comment on `pub mod llm;`, which makes
//! rustdoc resolve this block in `lib.rs` scope — the same trap the unresolved
//! `LlmBackend` link on the line above has been sitting in.)
//!
//! Since AILAB-196 it also owns the **pre-call** cost estimate behind
//! `cost_cap_usd`: [`crate::llm::estimate_prompt_cost`] prices the prompt with a
//! bundled tokenizer and [`crate::llm::exceeds_cap`] compares that to the
//! configured cap. Both are pure functions — they decide nothing. The abort
//! itself belongs to the caller, `dream_cycle::compose_lesson_body`, which is
//! the only place that knows a model call is otherwise about to happen and is
//! the only place that can turn "over cap" into a deterministic body. Keeping
//! the arithmetic here and the decision there is what lets the estimate be unit
//! tested without a backend, and what keeps the count from being re-run per
//! retry attempt.
//!
//! Since AILAB-199 it also owns [`crate::llm::load_personal_prompt_context`],
//! the reader that turns `.agent/personal/` into prompt bytes. The reader is
//! here rather than in `dream_cycle` for the same reason the cost estimate is:
//! it is a pure input-shaping function with no authority to run. Whether it is
//! *called* is `dream_cycle::compose_lesson_body`'s decision, gated on the
//! per-call `--share-personal` consent, and default-off.
//!
//! ## What this module does NOT own
//!
//! * Redaction of the JSONL event bodies it forwards. `build_lesson_prompt`
//!   still includes them verbatim — AILAB-199 governs the **personal layer**,
//!   which this module now excludes by default and appends only on consent.
//!
//! ## Credentials
//!
//! Environment first (`ANTHROPIC_API_KEY`, then `OPENAI_API_KEY`), else
//! `~/.config/dreamd/secrets.toml` at mode `0600`. A world- or group-readable
//! secrets file is **ignored with a warning**, never parsed. There is no OS
//! keychain path at v0.1.1 (`docs/v0.1.1-scope.md` §6), and this module never
//! writes or chmods the secrets file.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dreamd_protocol::{AgentLearning, EventId};
use serde::Deserialize;

use crate::config::Config;
use crate::layout::AgentRoot;

/// `prompt_version` written to `LESSONS.md` frontmatter when the body came from
/// the model (AILAB-201).
///
/// The date is the **bundle** date of [`LESSON_PROMPT`], not the cycle date: two
/// stores that composed with the same binary carry the same id, so a lesson's
/// frontmatter names the prompt that produced it rather than the night it ran.
/// The two halves move independently: the id names a prompt *revision*, the file
/// name names a prompt *file*. AILAB-200 rewrote the last rule of `v1.1.txt` in
/// place and moved the id to `v1.2` without renaming the file — one file can
/// back several ids. Either way the id changes in the same commit as the bytes,
/// or frontmatter starts lying about its own provenance.
///
/// The deterministic arm stamps [`crate::consolidation::DETERMINISTIC_PROMPT_ID`]
/// instead — no model, no prompt, no version to carry.
pub const VERSIONED_PROMPT_ID: &str = "dream-cycle/v1.2@2026-08-23";

/// INFO line emitted when the cycle falls back for lack of credentials.
///
/// The wording is the exact Linear AC phrase and is asserted by tests — it is
/// emitted even when the trigger is a missing/ill-permissioned `secrets.toml`
/// rather than a missing env var, because to the operator the two are the same
/// condition: dreamd has no key it is willing to use.
pub const NO_API_KEY_FALLBACK: &str = "no API key — falling back to --no-llm";

/// Per-attempt wall-clock budget. Enforced twice on the production path: once by
/// genai's own `WebConfig::with_timeout` (covers connect + response), and once
/// by [`complete_with_retries`] so the contract holds for **any** backend.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Attempts per composition, inclusive of the first. Only transport failures are
/// retried; an empty completion is a semantic answer, not a flaky one.
pub const MAX_ATTEMPTS: u32 = 3;

/// A successful completion plus whatever usage the provider reported.
#[derive(Debug, Clone)]
pub struct LlmCompletion {
    pub text: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

/// Why a completion did not happen. Both variants mean "fall back", never "fail
/// the cycle" — see the module docs.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("llm request failed: {0}")]
    Transport(String),
    #[error("llm empty completion")]
    Empty,
}

/// One text completion against `model`.
///
/// Return-position `impl Future` (not `async fn` in a `dyn`-safe shape) is
/// deliberate: the trait is only ever used through generics, so the extra
/// allocation of a boxed future buys nothing. It also means `LlmBackend` is not
/// object-safe — pass backends by generic parameter, not `&dyn LlmBackend`.
pub trait LlmBackend: Send + Sync {
    fn complete(
        &self,
        model: &str,
        prompt: &str,
    ) -> impl Future<Output = Result<LlmCompletion, LlmError>> + Send;
}

// ── Credentials ──────────────────────────────────────────────────────────────

/// Which provider a resolved key implies. The key variable / TOML field is the
/// only provider signal at this layer; `Config.provider` may still override the
/// adapter downstream in [`GenaiBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyProvider {
    Anthropic,
    OpenAi,
}

/// Where a key came from. Carried for the INFO log only — never logged with the
/// key itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Env,
    SecretsToml,
}

/// A usable API key. `value` is secret: it is never `Debug`-logged by this
/// module, and `Debug` is derived only so tests can assert on `provider`/`source`.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedKey {
    pub provider: KeyProvider,
    pub value: String,
    pub source: KeySource,
}

/// Redacts `value` — a `ResolvedKey` must stay safe to interpolate into a log
/// line or a panic message.
impl std::fmt::Debug for ResolvedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedKey")
            .field("provider", &self.provider)
            .field("source", &self.source)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// `~/.config/dreamd/secrets.toml` — the same platform config dir
/// [`crate::config`] uses, so systemd/launchd units resolve the same path.
/// `None` if the platform has no resolvable config dir.
pub fn secrets_toml_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("dreamd").join("secrets.toml"))
}

/// Snake_case key names, matching `docs/v0.1.1-scope.md` §6.
#[derive(Debug, Default, Deserialize)]
struct SecretsToml {
    anthropic_api_key: Option<String>,
    openai_api_key: Option<String>,
}

/// Resolve an API key: environment first, then `secrets.toml`, else `None`.
///
/// `None` is the signal to run the deterministic cycle — callers log
/// [`NO_API_KEY_FALLBACK`] and carry on.
pub fn resolve_llm_credentials() -> Option<ResolvedKey> {
    resolve_credentials_from(
        std::env::var("ANTHROPIC_API_KEY").ok(),
        std::env::var("OPENAI_API_KEY").ok(),
        secrets_toml_path().as_deref(),
    )
}

/// Pure resolution core. Takes the env values as arguments rather than reading
/// them so unit tests never mutate process-global state — `std::env::set_var`
/// is racy under cargo's multi-threaded test runner and would make the
/// precedence tests order-dependent.
fn resolve_credentials_from(
    anthropic_env: Option<String>,
    openai_env: Option<String>,
    secrets_path: Option<&Path>,
) -> Option<ResolvedKey> {
    // 1. Environment. Empty-string values count as unset: an exported-but-blank
    //    key is a misconfiguration, and treating it as present would send an
    //    unauthenticated request and burn the retry budget on 401s.
    if let Some(v) = anthropic_env.filter(|v| !v.is_empty()) {
        return Some(ResolvedKey {
            provider: KeyProvider::Anthropic,
            value: v,
            source: KeySource::Env,
        });
    }
    if let Some(v) = openai_env.filter(|v| !v.is_empty()) {
        return Some(ResolvedKey {
            provider: KeyProvider::OpenAi,
            value: v,
            source: KeySource::Env,
        });
    }

    // 2. secrets.toml, mode 0600 only.
    let secrets = read_secrets_toml(secrets_path?)?;
    if let Some(v) = secrets.anthropic_api_key.filter(|v| !v.is_empty()) {
        return Some(ResolvedKey {
            provider: KeyProvider::Anthropic,
            value: v,
            source: KeySource::SecretsToml,
        });
    }
    if let Some(v) = secrets.openai_api_key.filter(|v| !v.is_empty()) {
        return Some(ResolvedKey {
            provider: KeyProvider::OpenAi,
            value: v,
            source: KeySource::SecretsToml,
        });
    }
    None
}

/// Read and parse `secrets.toml`, refusing anything not at mode `0600` on unix.
///
/// Every failure is `None` (absent), never an error: a malformed or
/// over-permissive secrets file must degrade to the deterministic cycle, not
/// abort it. The permission refusal is deliberately *before* the read, so a
/// world-readable file's contents never enter this process's memory.
fn read_secrets_toml(path: &Path) -> Option<SecretsToml> {
    let meta = std::fs::metadata(path).ok()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{mode:04o}"),
                "ignoring secrets.toml: permissions must be 0600 (run `chmod 600` to enable it)"
            );
            return None;
        }
    }
    #[cfg(not(unix))]
    let _ = meta;

    let raw = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<SecretsToml>(&raw) {
        Ok(s) => Some(s),
        Err(e) => {
            // The error is rendered without the file body — a TOML parse error
            // can quote the offending line, which may be the key itself.
            tracing::warn!(
                path = %path.display(),
                "ignoring secrets.toml: parse error at line {:?}",
                e.span().map(|s| s.start)
            );
            None
        }
    }
}

// ── Prompt ───────────────────────────────────────────────────────────────────

/// Versioned instruction block (AILAB-201). Shape is dictated by
/// `assignments/14-day-blitz.md` §"Lesson shape": rule first, one rejected
/// alternative when the cluster shows one, never invent `evt_` ids, at most
/// eight sentences.
///
/// The bytes live in a `.txt` file rather than a string literal so a prompt edit
/// is a reviewable one-file diff and so [`VERSIONED_PROMPT_ID`] names something a
/// reader can open. `include_str!` rather than a runtime read: an installed
/// `dreamd` has no source tree to read from, and a prompt that can go missing at
/// run time is a cycle that fails for a reason the operator cannot fix.
///
/// A unit snapshot makes an edit here **loud**, not impossible — it fails review,
/// but `cargo insta accept` will still land it with [`VERSIONED_PROMPT_ID`]
/// stale. Moving the id is a reviewer's job, not the test harness's.
pub const LESSON_PROMPT: &str = include_str!("prompts/v1.1.txt");

/// Build the composition prompt for one promoted cluster.
///
/// **Cluster-only, always.** Event bodies are included verbatim; nothing from
/// `.agent/personal/` can reach this string, because this function is never
/// given the store to read it from. The opt-in personal context (AILAB-199) is
/// appended by `dream_cycle::compose_lesson_body` *after* this call, so the
/// default prompt is the one below whatever the operator's `personal/` holds.
pub fn build_lesson_prompt(cluster_key: &str, events: &[AgentLearning]) -> String {
    let mut s = String::with_capacity(LESSON_PROMPT.len() + 256 * events.len());
    s.push_str(LESSON_PROMPT);
    s.push_str("\n\nCluster: ");
    s.push_str(cluster_key);
    s.push_str("\n\nEvents:\n");
    for event in events {
        s.push_str("- ");
        s.push_str(event.id.as_str());
        s.push_str(" (");
        s.push_str(&event.skill_action);
        s.push_str("): ");
        s.push_str(&event.content);
        s.push('\n');
    }
    s
}

// ── Personal layer (AILAB-199 / DR-310) ──────────────────────────────────────

/// Byte cap on personal-layer content handed to one consumer in one shot.
///
/// The same 16 KiB `GET /api/v1/preferences` has always truncated at — that
/// handler now re-exports this constant as `PREFERENCES_SIZE_CAP` rather than
/// restating the number, so the prompt budget and the HTTP budget cannot drift
/// apart. Deliberately small next to `cost_cap_usd`: personal context is
/// seasoning on a cluster prompt, and a `personal/` that has grown into a wiki
/// must not be able to push an otherwise-affordable cycle over the AILAB-196
/// cap as a side effect of opting in.
pub const PERSONAL_LAYER_MAX_BYTES: usize = 16 * 1024; // 16 KiB

/// Heading that introduces opted-in personal bytes in the composition prompt.
///
/// Load-bearing rather than decorative. Without it, a canary string in
/// `PREFERENCES.md` is indistinguishable from an event body that happens to
/// repeat the same words, so the default-exclude test could pass against a
/// prompt that had in fact leaked. The heading also tells the model which half
/// of its context is the operator speaking and which half is the log.
pub const PERSONAL_CONTEXT_HEADING: &str = "Personal (operator opted in with --share-personal):";

/// Bytes from `.agent/personal/` to append to the composition prompt.
///
/// `None` = attach nothing, which is the default, the `--no-llm` path, and the
/// answer for any store whose `personal/` is absent, empty, or entirely skipped.
/// Calling this at all is consent (`--share-personal` / `x-dreamd-share-personal:
/// 1`); the function itself has no opinion on whether it should have been called,
/// which is why the gate lives at the one call site in
/// `dream_cycle::compose_lesson_body` rather than being re-checked here.
///
/// Reads **only** regular UTF-8 files directly under `personal_dir()`. Each
/// entry is canonicalized and then proven to still live under the canonical
/// `personal/` before it is opened, so a symlink planted in that directory
/// cannot pull `~/.ssh/id_ed25519` into a prompt. Directories are skipped (no
/// recursion — one flat layer is the documented shape), and so is any file whose
/// bytes are not UTF-8, with a WARN naming it so a skipped file is visible
/// rather than silently missing from the prompt the operator asked for.
///
/// Order is `PREFERENCES.md`, then `DECISIONS.md`, then everything else by file
/// name: the two named files are the layer's documented contents, and putting
/// them first keeps the head of the block stable as incidental files come and
/// go. Total output is capped at [`PERSONAL_LAYER_MAX_BYTES`] and truncated with
/// a WARN when over — the prompt is priced immediately after this returns
/// (AILAB-196), so an unbounded read here would be an unbounded bill.
pub fn load_personal_prompt_context(agent_root: &AgentRoot) -> Option<String> {
    // Canonical `personal/` is both the directory we read and the containment
    // yardstick below. `personal/` being a symlink itself is fine; a *member*
    // resolving outside it is not.
    let canonical_dir = std::fs::canonicalize(agent_root.personal_dir()).ok()?;

    // (rank, file_name, resolved_path) — rank pins the two documented files to
    // the front, file_name breaks ties so the block is deterministic across
    // filesystems that hand back `read_dir` in arbitrary order.
    let mut files: Vec<(u8, std::ffi::OsString, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&canonical_dir).ok()?.flatten() {
        let path = entry.path();
        let Ok(resolved) = std::fs::canonicalize(&path) else {
            // Dangling symlink or a race with a delete. Nothing to read.
            continue;
        };
        if !resolved.starts_with(&canonical_dir) {
            tracing::warn!(
                path = %path.display(),
                "personal/ entry resolves outside the personal layer; not shared with the model"
            );
            continue;
        }
        if !resolved.is_file() {
            continue;
        }
        let name = entry.file_name();
        let rank = match name.to_str() {
            Some("PREFERENCES.md") => 0,
            Some("DECISIONS.md") => 1,
            _ => 2,
        };
        files.push((rank, name, resolved));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut out = String::new();
    for (_, name, path) in &files {
        let Ok(bytes) = std::fs::read(path) else {
            tracing::warn!(path = %path.display(), "personal/ file unreadable; skipped");
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            tracing::warn!(path = %path.display(), "personal/ file is not UTF-8; skipped");
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("### ");
        out.push_str(&name.to_string_lossy());
        out.push('\n');
        out.push_str(text.trim_end());
        out.push('\n');
    }

    if out.is_empty() {
        return None;
    }
    if out.len() > PERSONAL_LAYER_MAX_BYTES {
        // `truncate` panics off a char boundary, and a multi-byte grapheme
        // straddling the cap is the normal case for prose, not an exotic one.
        let mut end = PERSONAL_LAYER_MAX_BYTES;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        tracing::warn!(
            original_size = out.len(),
            cap = PERSONAL_LAYER_MAX_BYTES,
            "personal/ context truncated to the 16 KiB cap before pricing the prompt"
        );
        out.truncate(end);
    }
    Some(out)
}

// ── Cost estimate (AILAB-196 / DR-307) ───────────────────────────────────────

/// Completion-token reserve. The prompt caps the lesson at 8 sentences; 512
/// tokens is the conservative pre-call pad. Errs toward abort, not surprise bills.
///
/// A pre-call estimate cannot know the completion length, so the choice is
/// between under-reserving (the cap passes and the bill arrives anyway) and
/// over-reserving (a borderline cycle composes deterministically). Only the
/// second failure mode is recoverable by the operator — raise `cost_cap_usd` —
/// so the pad is deliberately larger than any lesson this prompt can produce.
pub const OUTPUT_RESERVE_TOKENS: u32 = 512;

/// USD per 1,000 tokens. Snapshotted 2026-08-24. Refresh the table and
/// `docs/llm-cost-accuracy.md` in the same commit.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRates {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
}

/// Published price for `model`, or `None` for a model this build has never been
/// told the price of.
///
/// The table is a hand-maintained **snapshot taken 2026-08-24**, not a live
/// lookup: pricing lives behind a provider dashboard, and a cap that silently
/// re-prices itself over the network is a cap the operator cannot reason about.
/// The cost of that choice is that the table goes stale in place, so refreshing
/// it is a three-part edit that belongs in **one commit**: the match arms here,
/// the ±drift/pricing prose in `docs/llm-cost-accuracy.md`, and the snapshot
/// date in both. Split them and the doc starts describing rates the binary no
/// longer charges against.
///
/// Aliases are spelled out rather than normalized because the model string is
/// operator-typed config, not a parsed enum: `claude-haiku-4.5` is what the
/// pricing page prints and `claude-haiku-4-5` is what the API accepts, and a
/// typo'd third spelling should fall through to `None` (over-cap → deterministic)
/// rather than silently price as something else.
pub fn rates_for(model: &str) -> Option<ModelRates> {
    match model {
        "claude-haiku-4-5" | "claude-haiku-4.5" => Some(ModelRates {
            input_per_1k: 0.001,  // $1 / 1M input
            output_per_1k: 0.005, // $5 / 1M output
        }),
        "gpt-4o-mini" => Some(ModelRates {
            input_per_1k: 0.00015,
            output_per_1k: 0.0006,
        }),
        _ => None,
    }
}

/// What one composition is predicted to cost, before it is attempted.
///
/// `input_tokens` is carried alongside the dollar figure so the abort log can
/// say *why* a prompt was expensive — a cap breach caused by a 40k-token cluster
/// is an operator problem with a different fix than one caused by a raised
/// price.
#[derive(Debug, Clone, PartialEq)]
pub struct CostEstimate {
    pub input_tokens: u32,
    pub estimated_usd: f64,
}

/// Pre-call USD estimate for `prompt` under `model`.
///
/// The caller compares `estimated_usd` to [`crate::config::Config::cost_cap_usd`].
/// `None` = unknown model (caller aborts to deterministic).
///
/// Tokens are counted with `cl100k_base` for **every** model, Anthropic
/// included. That is knowingly the wrong tokenizer for Claude — Anthropic's
/// segmentation differs by roughly ±25% on prose of this shape — and it is the
/// deliberate trade: the alternatives are a network round trip to the provider's
/// own token-counting endpoint (a remote call to decide whether to make a remote
/// call, on the exact path that is supposed to be cheap) or a vendored copy of a
/// tokenizer the provider does not publish. Because the reserve pad in
/// [`OUTPUT_RESERVE_TOKENS`] is
/// already generous, the combined error lands on the safe side: the cap errs
/// toward abort, and the documented worst case is real spend near ~$0.13
/// against a $0.10 cap (`docs/llm-cost-accuracy.md`).
///
/// Building the encoder is fallible, and this module's whole contract is that it
/// degrades rather than panics — a `dream` cycle must not die because a
/// tokenizer table failed to load. So an encoder error returns `None`, which the
/// caller reads exactly like an unknown model: treat as over cap, compose
/// deterministically. `unwrap()` here would convert a cheap fallback into a
/// crashed cycle.
pub fn estimate_prompt_cost(model: &str, prompt: &str) -> Option<CostEstimate> {
    let rates = rates_for(model)?;

    let bpe = match tiktoken_rs::cl100k_base() {
        Ok(bpe) => bpe,
        Err(err) => {
            tracing::warn!(
                model = %model,
                error = %err,
                "cl100k_base tokenizer unavailable — treating cost estimate as over cap"
            );
            return None;
        }
    };
    let input_tokens = bpe.encode_with_special_tokens(prompt).len() as u32;

    let estimated_usd = (f64::from(input_tokens) / 1000.0) * rates.input_per_1k
        + (f64::from(OUTPUT_RESERVE_TOKENS) / 1000.0) * rates.output_per_1k;

    Some(CostEstimate {
        input_tokens,
        estimated_usd,
    })
}

/// Whether `estimate` is refused by a `cap_usd` budget.
///
/// A non-positive cap is *always* a breach rather than "no limit": `0.0` is what
/// an operator writes to mean "never call the model", and reading it as unlimited
/// would invert the one setting whose failure mode costs money.
pub fn exceeds_cap(estimate: &CostEstimate, cap_usd: f64) -> bool {
    cap_usd <= 0.0 || estimate.estimated_usd > cap_usd
}

// ── Citation gate (AILAB-200 / DR-306) ───────────────────────────────────────

/// Trailer key the prompt asks the model to end with.
const CITATIONS_PREFIX: &str = "citations:";

/// Split model text into prose + citation ids.
///
/// The trailer must be the **last non-empty line** and must read
/// `citations: evt_aaa evt_bbb` (space-separated, `evt_` + a 26-char Crockford
/// ULID, validated by [`dreamd_protocol::EventId::parse`]). Missing or malformed
/// → `None`, which the caller turns into a deterministic body, never a cycle
/// error.
///
/// Only the trailer line is read. Scanning the prose for `evt_` substrings would
/// let a lesson that *mentions* an id in passing pass the gate this exists to be,
/// and would pin events the model never claimed to have drawn on.
pub fn split_citation_trailer(text: &str) -> Option<(String, Vec<String>)> {
    let lines: Vec<&str> = text.lines().collect();
    let trailer_idx = lines.iter().rposition(|l| !l.trim().is_empty())?;

    let ids: Vec<String> = lines[trailer_idx]
        .trim()
        .strip_prefix(CITATIONS_PREFIX)?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if ids.is_empty() || !ids.iter().all(|id| EventId::parse(id).is_ok()) {
        return None;
    }

    // Everything before the trailer is the lesson. A model that emitted only a
    // trailer yields empty prose; the caller's empty-body rule (AILAB-204)
    // catches that downstream.
    let prose = lines[..trailer_idx].join("\n").trim().to_string();

    // A *second* `citations:` line above the trailer is not prose. Stripping only
    // the last one would store an unvalidated `citations: evt_…` line as the
    // lesson body, which §1.5 forbids — and those ids never pass through
    // `citations_are_valid`, so a hallucinated id would survive as indexed text.
    // A doubled trailer is malformed input: fall back like any other.
    if prose
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.trim().starts_with(CITATIONS_PREFIX))
    {
        return None;
    }

    Some((prose, ids))
}

/// Every cited id must be a member of the promoted cluster, and there must be at
/// least one.
///
/// `cluster_ids` is `SelectedLesson::events`, not the whole JSONL: a real id from
/// some other cluster is still a hallucinated *citation* for this lesson, and
/// pinning it would keep an unrelated event alive through decay.
pub fn citations_are_valid(cited: &[String], cluster_ids: &[String]) -> bool {
    !cited.is_empty() && cited.iter().all(|id| cluster_ids.iter().any(|c| c == id))
}

// ── Retry driver ─────────────────────────────────────────────────────────────

/// Run `backend` with [`MAX_ATTEMPTS`] attempts and a [`REQUEST_TIMEOUT`] budget
/// on each, returning the first non-empty completion.
///
/// Only [`LlmError::Transport`] is retried. [`LlmError::Empty`] short-circuits:
/// the provider answered, it just answered with nothing, and three identical
/// round-trips would not change that. After the final transport failure the
/// error is returned so the caller can fall back — this function never panics
/// and never blocks past `MAX_ATTEMPTS * REQUEST_TIMEOUT`.
pub async fn complete_with_retries<B: LlmBackend>(
    backend: &B,
    model: &str,
    prompt: &str,
) -> Result<LlmCompletion, LlmError> {
    let mut last_transport = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        let outcome =
            match tokio::time::timeout(REQUEST_TIMEOUT, backend.complete(model, prompt)).await {
                Ok(result) => result,
                Err(_elapsed) => Err(LlmError::Transport(format!(
                    "timed out after {}s",
                    REQUEST_TIMEOUT.as_secs()
                ))),
            };

        match outcome {
            Ok(completion) if completion.text.trim().is_empty() => return Err(LlmError::Empty),
            Ok(completion) => {
                tracing::info!(
                    model,
                    input_tokens = completion.input_tokens,
                    output_tokens = completion.output_tokens,
                    attempt,
                    "llm completion ok"
                );
                return Ok(completion);
            }
            Err(LlmError::Empty) => return Err(LlmError::Empty),
            Err(LlmError::Transport(e)) => {
                tracing::warn!(
                    model,
                    attempt,
                    max_attempts = MAX_ATTEMPTS,
                    error = %e,
                    "llm attempt failed"
                );
                last_transport = e;
            }
        }
    }

    Err(LlmError::Transport(last_transport))
}

// ── Production backend ───────────────────────────────────────────────────────

/// Production [`LlmBackend`] over the `genai` crate.
///
/// One `genai::Client` per cycle. The API key is injected through an
/// `AuthResolver` rather than left to genai's own env lookup, because a key may
/// have come from `secrets.toml`, which genai knows nothing about.
pub struct GenaiBackend {
    client: genai::Client,
}

impl std::fmt::Debug for GenaiBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The client holds the API key; never derive Debug on it.
        f.debug_struct("GenaiBackend").finish_non_exhaustive()
    }
}

/// Map the `provider` config key onto a genai adapter. `None` means "let genai
/// infer from the model name" — which is the default (`provider = ""`) and the
/// correct answer for `claude-*` / `gpt-*` model ids.
fn adapter_for_provider(provider: &str) -> Option<genai::adapter::AdapterKind> {
    use genai::adapter::AdapterKind;
    match provider.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "anthropic" => Some(AdapterKind::Anthropic),
        "openai" => Some(AdapterKind::OpenAI),
        other => {
            tracing::warn!(
                provider = other,
                "unknown provider in config; inferring from the model name instead"
            );
            None
        }
    }
}

impl GenaiBackend {
    /// Build a client bound to `key`, honoring `config.provider` and
    /// `config.endpoint` when they are non-empty.
    pub fn new(key: &ResolvedKey, config: &Config) -> Self {
        use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
        use genai::{ModelIden, ServiceTarget};

        let mut builder = genai::Client::builder()
            .with_web_config(genai::WebConfig::default().with_timeout(REQUEST_TIMEOUT));

        // Auth runs before the service-target resolver (genai `ClientConfig::
        // resolve_service_target`), so the target handed to the resolver below
        // already carries this key.
        let auth_value = key.value.clone();
        builder = builder.with_auth_resolver_fn(
            move |_model: ModelIden| -> Result<Option<AuthData>, genai::resolver::Error> {
                Ok(Some(AuthData::from_single(auth_value.clone())))
            },
        );

        let forced_adapter = adapter_for_provider(&config.provider);
        let endpoint_override = config.endpoint.trim().to_string();
        if forced_adapter.is_some() || !endpoint_override.is_empty() {
            builder =
                builder.with_service_target_resolver(ServiceTargetResolver::from_resolver_fn(
                    move |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                        let ServiceTarget {
                            mut endpoint,
                            auth,
                            mut model,
                        } = target;
                        if let Some(kind) = forced_adapter {
                            model = ModelIden::new(kind, model.model_name);
                        }
                        if !endpoint_override.is_empty() {
                            endpoint = Endpoint::from_owned(endpoint_override.clone());
                        }
                        Ok(ServiceTarget {
                            endpoint,
                            auth,
                            model,
                        })
                    },
                ));
        }

        GenaiBackend {
            client: builder.build(),
        }
    }
}

impl LlmBackend for GenaiBackend {
    async fn complete(&self, model: &str, prompt: &str) -> Result<LlmCompletion, LlmError> {
        use genai::chat::{ChatMessage, ChatRequest};

        let request = ChatRequest::default().append_message(ChatMessage::user(prompt));
        let response = self
            .client
            .exec_chat(model, request, None)
            .await
            // `genai::Error` renders the model iden and the transport failure;
            // it does not include the API key or the request body.
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        let text = response.first_text().unwrap_or_default().to_string();
        if text.trim().is_empty() {
            return Err(LlmError::Empty);
        }
        Ok(LlmCompletion {
            text,
            input_tokens: response
                .usage
                .prompt_tokens
                .and_then(|t| u32::try_from(t).ok()),
            output_tokens: response
                .usage
                .completion_tokens
                .and_then(|t| u32::try_from(t).ok()),
        })
    }
}

/// Resolve the production backend for this cycle, or `None` to run deterministic.
///
/// `None` is returned — and [`NO_API_KEY_FALLBACK`] logged at INFO — when no
/// usable credential exists. This is the single place that decision is made on
/// the production path, so the AC phrase has exactly one emitter.
pub fn resolve_backend(config: &Config) -> Option<GenaiBackend> {
    let key = match resolve_llm_credentials() {
        Some(k) => k,
        None => {
            tracing::info!("{NO_API_KEY_FALLBACK}");
            return None;
        }
    };
    tracing::debug!(
        provider = ?key.provider,
        source = ?key.source,
        model = %config.model,
        "llm credentials resolved"
    );
    Some(GenaiBackend::new(&key, config))
}

/// Scripted [`LlmBackend`] for tests — no network, no credentials.
///
/// Lives outside `mod tests` because the dream-cycle composition tests
/// (`crate::dream_cycle`) drive the same seam and need the same double.
/// Compiled only under `cfg(test)`; never linked into a release artifact.
#[cfg(test)]
pub(crate) mod fake {
    use super::{LlmBackend, LlmCompletion, LlmError};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    pub(crate) struct FakeLlm {
        text: Option<String>,
        fail_with: Option<String>,
        /// Attempt counter, so a test can assert the retry budget was honored
        /// rather than merely that the final value was right.
        pub(crate) calls: AtomicU32,
        /// The prompt of the most recent attempt (AILAB-199).
        ///
        /// Recorded rather than ignored because "no `personal/` content appears
        /// in the LLM request payload" is only provable against the bytes the
        /// backend was actually handed. Asserting on the composed *lesson*
        /// would prove nothing: the model here is a fake that never reads its
        /// input, so a leaking prompt and a clean one produce identical output.
        /// `Mutex` rather than a channel or a `RefCell` — `LlmBackend: Sync`
        /// and `complete` takes `&self`, and this is a std type, not a dep.
        pub(crate) last_prompt: Mutex<Option<String>>,
    }

    impl FakeLlm {
        /// Always answers with `text` (which may be empty/whitespace).
        pub(crate) fn ok(text: &str) -> Self {
            FakeLlm {
                text: Some(text.to_string()),
                fail_with: None,
                calls: AtomicU32::new(0),
                last_prompt: Mutex::new(None),
            }
        }
        /// Always fails with a transport error — the retried variant.
        pub(crate) fn failing(msg: &str) -> Self {
            FakeLlm {
                text: None,
                fail_with: Some(msg.to_string()),
                calls: AtomicU32::new(0),
                last_prompt: Mutex::new(None),
            }
        }

        /// The prompt of the last attempt, or `None` if the backend was never
        /// called. A test that expects zero calls asserts on `None` here and on
        /// `calls == 0`, which are two independent witnesses to the same claim.
        pub(crate) fn last_prompt(&self) -> Option<String> {
            self.last_prompt
                .lock()
                .expect("FakeLlm prompt mutex is never held across a panic")
                .clone()
        }
    }

    impl LlmBackend for FakeLlm {
        async fn complete(&self, _model: &str, prompt: &str) -> Result<LlmCompletion, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Record BEFORE the failure arm: a retried transport error still
            // sent these bytes, so a leak on a failing call is still a leak.
            *self
                .last_prompt
                .lock()
                .expect("FakeLlm prompt mutex is never held across a panic") =
                Some(prompt.to_string());
            if let Some(msg) = &self.fail_with {
                return Err(LlmError::Transport(msg.clone()));
            }
            Ok(LlmCompletion {
                text: self.text.clone().unwrap_or_default(),
                input_tokens: Some(11),
                output_tokens: Some(7),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeLlm;
    use super::*;
    use crate::test_support::{unique_tmpdir, DirGuard};
    use std::sync::atomic::Ordering;

    // ── Credentials ──────────────────────────────────────────────────────────

    #[test]
    fn env_key_wins_over_secrets_toml() {
        let dir = unique_tmpdir("llm-env-wins");
        let _g = DirGuard(dir.clone());
        let secrets = dir.join("secrets.toml");
        write_secrets(&secrets, "anthropic_api_key = \"from-file\"\n", 0o600);

        let got =
            resolve_credentials_from(Some("from-env".to_string()), None, Some(secrets.as_path()))
                .expect("env key resolves");

        assert_eq!(got.value, "from-env");
        assert_eq!(got.provider, KeyProvider::Anthropic);
        assert_eq!(got.source, KeySource::Env);
    }

    #[test]
    fn anthropic_env_beats_openai_env() {
        let got = resolve_credentials_from(
            Some("anthropic".to_string()),
            Some("openai".to_string()),
            None,
        )
        .expect("resolves");
        assert_eq!(got.provider, KeyProvider::Anthropic);
        assert_eq!(got.value, "anthropic");
    }

    #[test]
    fn empty_env_value_is_treated_as_unset() {
        let got = resolve_credentials_from(Some(String::new()), Some("oai".to_string()), None)
            .expect("falls through the blank anthropic key");
        assert_eq!(got.provider, KeyProvider::OpenAi);
        assert_eq!(got.value, "oai");
    }

    #[test]
    fn secrets_toml_at_0600_is_read() {
        let dir = unique_tmpdir("llm-secrets-0600");
        let _g = DirGuard(dir.clone());
        let secrets = dir.join("secrets.toml");
        write_secrets(&secrets, "openai_api_key = \"sk-from-file\"\n", 0o600);

        let got = resolve_credentials_from(None, None, Some(secrets.as_path()))
            .expect("0600 secrets.toml is readable");
        assert_eq!(got.value, "sk-from-file");
        assert_eq!(got.provider, KeyProvider::OpenAi);
        assert_eq!(got.source, KeySource::SecretsToml);
    }

    #[cfg(unix)]
    #[test]
    fn secrets_toml_at_0644_is_ignored() {
        let dir = unique_tmpdir("llm-secrets-0644");
        let _g = DirGuard(dir.clone());
        let secrets = dir.join("secrets.toml");
        write_secrets(&secrets, "anthropic_api_key = \"leaky\"\n", 0o644);

        assert!(
            resolve_credentials_from(None, None, Some(secrets.as_path())).is_none(),
            "world-readable secrets.toml must be ignored, not parsed"
        );
    }

    #[test]
    fn missing_secrets_toml_is_none() {
        let dir = unique_tmpdir("llm-secrets-missing");
        let _g = DirGuard(dir.clone());
        let secrets = dir.join("does-not-exist.toml");
        assert!(resolve_credentials_from(None, None, Some(secrets.as_path())).is_none());
        assert!(resolve_credentials_from(None, None, None).is_none());
    }

    #[test]
    fn secrets_toml_without_keys_is_none() {
        let dir = unique_tmpdir("llm-secrets-emptykeys");
        let _g = DirGuard(dir.clone());
        let secrets = dir.join("secrets.toml");
        write_secrets(&secrets, "anthropic_api_key = \"\"\n", 0o600);
        assert!(resolve_credentials_from(None, None, Some(secrets.as_path())).is_none());
    }

    #[test]
    fn resolved_key_debug_redacts_the_value() {
        let key = ResolvedKey {
            provider: KeyProvider::Anthropic,
            value: "sk-super-secret".to_string(),
            source: KeySource::Env,
        };
        let rendered = format!("{key:?}");
        assert!(
            !rendered.contains("sk-super-secret"),
            "Debug leaked the key: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }

    // ── Retry driver ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fake_llm_success_returns_text_on_first_attempt() {
        let fake = FakeLlm::ok("Prefer `?` over `.unwrap()` outside tests.");
        let got = complete_with_retries(&fake, "claude-haiku-4-5", "prompt")
            .await
            .expect("success");
        assert_eq!(got.text, "Prefer `?` over `.unwrap()` outside tests.");
        assert_eq!(got.input_tokens, Some(11));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1, "no needless retry");
    }

    #[tokio::test]
    async fn fake_llm_transport_failure_retries_three_times_then_errors() {
        let fake = FakeLlm::failing("connection reset");
        let err = complete_with_retries(&fake, "claude-haiku-4-5", "prompt")
            .await
            .expect_err("three transport failures must surface as Err");
        assert!(matches!(err, LlmError::Transport(_)), "got {err:?}");
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            MAX_ATTEMPTS,
            "driver must make exactly MAX_ATTEMPTS attempts"
        );
    }

    #[tokio::test]
    async fn empty_completion_is_err_empty_without_retrying() {
        let fake = FakeLlm::ok("   \n  ");
        let err = complete_with_retries(&fake, "claude-haiku-4-5", "prompt")
            .await
            .expect_err("whitespace-only completion is Empty");
        assert!(matches!(err, LlmError::Empty), "got {err:?}");
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "an empty answer is not a flaky one — do not retry it"
        );
    }

    // ── Prompt / constants ───────────────────────────────────────────────────

    #[test]
    fn prompt_carries_cluster_and_every_event_id() {
        let events = vec![
            learning(1, "rust::error_handling", "unwrap panicked in the handler"),
            learning(2, "rust::error_handling", "switched to `?` and it held"),
        ];
        let prompt = build_lesson_prompt("rust::error_handling", &events);

        assert!(prompt.starts_with(LESSON_PROMPT));
        assert!(prompt.contains("Cluster: rust::error_handling"));
        assert!(prompt.contains(events[0].id.as_str()));
        assert!(prompt.contains(events[1].id.as_str()));
        assert!(prompt.contains("unwrap panicked in the handler"));
        // Shape contract from assignments/14-day-blitz.md.
        assert!(prompt.contains("Never invent an id."));
        assert!(prompt.contains("At most 8 sentences."));
        // AILAB-200: the trailer the citation gate parses is asked for by name.
        assert!(prompt.contains("citations: evt_aaa evt_bbb"));
    }

    /// Pins the bundled prompt body, so an edit to `src/prompts/v1.1.txt` shows
    /// up in review as a snapshot diff next to the [`VERSIONED_PROMPT_ID`] that
    /// is supposed to move with it.
    #[test]
    fn v1_1_prompt_body_is_pinned() {
        insta::assert_snapshot!("v1_1_prompt", LESSON_PROMPT);
    }

    /// insta normalizes with `trim_end` on both sides, so the snapshot above is
    /// blind to trailing whitespace — and trailing whitespace is exactly what
    /// [`build_lesson_prompt`] concatenates against. Pin the seam separately.
    #[test]
    fn v1_1_prompt_ends_with_exactly_one_newline() {
        assert!(
            LESSON_PROMPT.ends_with("with nothing after that line.\n"),
            "prompt must end with the final rule plus the POSIX newline"
        );
        assert!(
            !LESSON_PROMPT.ends_with("\n\n"),
            "a second trailing newline silently widens the gap before `Cluster:`"
        );
    }

    /// AILAB-200 rewrote the last rule of `v1.1.txt` in place, so the id moved
    /// to `v1.2` while the file name stayed. The snapshot above pins the bytes;
    /// this pins the id that must move with them.
    #[test]
    fn versioned_prompt_id_is_the_v1_2_bundle_id() {
        assert_eq!(VERSIONED_PROMPT_ID, "dream-cycle/v1.2@2026-08-23");
    }

    #[test]
    fn fallback_phrase_matches_the_linear_ac_verbatim() {
        assert_eq!(NO_API_KEY_FALLBACK, "no API key — falling back to --no-llm");
    }

    #[test]
    fn timeout_and_attempt_budget_match_the_spec() {
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(30));
        assert_eq!(MAX_ATTEMPTS, 3);
    }

    #[test]
    fn provider_key_maps_to_adapter_or_infers() {
        use genai::adapter::AdapterKind;
        assert_eq!(adapter_for_provider(""), None, "default infers from model");
        assert_eq!(
            adapter_for_provider("Anthropic"),
            Some(AdapterKind::Anthropic)
        );
        assert_eq!(adapter_for_provider("openai"), Some(AdapterKind::OpenAI));
        assert_eq!(
            adapter_for_provider("nope"),
            None,
            "unknown provider degrades to inference rather than failing the cycle"
        );
    }

    // ── Cost estimate (AILAB-196) ────────────────────────────────────────────

    /// USD comparisons are float arithmetic; a hundredth of a cent is far below
    /// any rate in the table, so it separates "wrong" from "last-bit rounding".
    const USD_EPS: f64 = 1e-9;

    /// The pad every model pays for up front, at haiku's output rate.
    const HAIKU_PAD_USD: f64 = 512.0 / 1000.0 * 0.005;

    #[test]
    fn a_known_prompt_counts_a_stable_nonzero_number_of_input_tokens() {
        let prompt = "Lead with the rule. Cite the events you used.";
        let first = estimate_prompt_cost("claude-haiku-4-5", prompt).expect("known model");
        let second = estimate_prompt_cost("claude-haiku-4-5", prompt).expect("known model");

        assert!(
            first.input_tokens > 0,
            "a non-empty prompt that counts as zero tokens means the encoder never ran"
        );
        assert_eq!(
            first.input_tokens, second.input_tokens,
            "the bundled encoding must be deterministic across calls"
        );
    }

    #[test]
    fn haiku_estimate_charges_the_reserved_output_pad_on_top_of_the_input_cost() {
        let prompt = build_lesson_prompt(
            "rust::error_handling",
            &[learning(1, "rust::error_handling", "unwrap panicked")],
        );
        let est = estimate_prompt_cost("claude-haiku-4-5", &prompt).expect("known model");

        let input_only = f64::from(est.input_tokens) / 1000.0 * 0.001;
        assert!(
            (est.estimated_usd - (input_only + HAIKU_PAD_USD)).abs() < USD_EPS,
            "estimate {} should be input {input_only} + pad {HAIKU_PAD_USD}",
            est.estimated_usd
        );
        assert!(
            est.estimated_usd > input_only,
            "an estimate that ignores OUTPUT_RESERVE_TOKENS would under-price every cycle"
        );
    }

    #[test]
    fn gpt_4o_mini_prices_the_same_prompt_lower_than_haiku() {
        let prompt = "Lead with the rule. Cite the events you used.";
        let haiku = estimate_prompt_cost("claude-haiku-4-5", prompt).expect("known model");
        let mini = estimate_prompt_cost("gpt-4o-mini", prompt).expect("known model");

        assert_eq!(
            haiku.input_tokens, mini.input_tokens,
            "cl100k_base is used for every model, so the token count cannot differ"
        );
        assert!(
            mini.estimated_usd < haiku.estimated_usd,
            "cheaper rates must produce a cheaper estimate: {} vs {}",
            mini.estimated_usd,
            haiku.estimated_usd
        );
    }

    #[test]
    fn the_dotted_haiku_alias_prices_identically_to_the_dashed_model_id() {
        assert_eq!(
            rates_for("claude-haiku-4.5"),
            rates_for("claude-haiku-4-5"),
            "the pricing-page spelling and the API spelling name one model"
        );

        let prompt = "Lead with the rule.";
        assert_eq!(
            estimate_prompt_cost("claude-haiku-4.5", prompt),
            estimate_prompt_cost("claude-haiku-4-5", prompt)
        );
    }

    #[test]
    fn an_unknown_model_string_yields_no_estimate_at_all() {
        assert!(rates_for("gpt-5-imaginary").is_none());
        assert!(
            estimate_prompt_cost("gpt-5-imaginary", "some prompt").is_none(),
            "an unpriced model must reach the caller as None, which it reads as over cap"
        );
    }

    #[test]
    fn exceeds_cap_refuses_only_estimates_above_the_budget() {
        let est = CostEstimate {
            input_tokens: 1_000,
            estimated_usd: 0.05,
        };

        assert!(exceeds_cap(&est, 0.04), "0.05 is over a 0.04 cap");
        assert!(
            !exceeds_cap(&est, 0.10),
            "0.05 is under the default 0.10 cap"
        );
        assert!(
            !exceeds_cap(&est, 0.05),
            "the cap is a ceiling, not an exclusive bound — exactly at cap still runs"
        );
    }

    #[test]
    fn a_zero_cap_is_over_budget_for_every_estimate() {
        let free = CostEstimate {
            input_tokens: 0,
            estimated_usd: 0.0,
        };
        assert!(
            exceeds_cap(&free, 0.0),
            "cap_usd = 0.0 means never call the model, not unlimited"
        );
        assert!(
            exceeds_cap(&free, -1.0),
            "a negative cap cannot authorize spend"
        );
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn write_secrets(path: &Path, body: &str, mode: u32) {
        std::fs::write(path, body).expect("write secrets fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("chmod secrets fixture");
        }
        #[cfg(not(unix))]
        let _ = mode;
    }

    /// `n` is padded into a valid `evt_<26-char Crockford ULID>`; `EventId::parse`
    /// rejects anything shorter, so tests cannot use bare `evt_aaa` literals.
    fn learning(n: u32, skill_action: &str, content: &str) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: dreamd_protocol::EventId::parse(&format!("evt_{n:0>26}")).unwrap(),
            timestamp: chrono::DateTime::from_timestamp(1_747_137_600, 0).unwrap(),
            pain: 5.0,
            importance: 5.0,
            pinned: false,
            skill_action: skill_action.to_string(),
            source_harness: "test".to_string(),
            content: content.to_string(),
        }
    }

    // ── Citation gate (AILAB-200) ────────────────────────────────────────────

    /// Real 26-char Crockford ids — `EventId::parse` rejects anything shorter,
    /// so the short `evt_1` style used elsewhere in the tree cannot be used here.
    const ID_A: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const ID_B: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAW";

    #[test]
    fn trailer_splits_prose_from_ids() {
        let text = format!("Lead with the rule.\nIt held.\n\ncitations: {ID_A} {ID_B}");
        let (prose, ids) = split_citation_trailer(&text).expect("well-formed trailer");

        assert_eq!(prose, "Lead with the rule.\nIt held.");
        assert_eq!(ids, vec![ID_A.to_string(), ID_B.to_string()]);
    }

    #[test]
    fn trailer_tolerates_trailing_blank_lines_and_padding() {
        let text = format!("Prose.\n\n   citations:   {ID_A}   \n\n  \n");
        let (prose, ids) =
            split_citation_trailer(&text).expect("last non-empty line is the trailer");
        assert_eq!(prose, "Prose.");
        assert_eq!(ids, vec![ID_A.to_string()]);
    }

    #[test]
    fn missing_trailer_is_none() {
        assert!(
            split_citation_trailer("Just prose, no trailer.").is_none(),
            "a model that never emitted a trailer must not parse as one"
        );
    }

    #[test]
    fn ids_mentioned_only_in_the_prose_do_not_count() {
        // The gate reads the trailer line and nothing else. Scraping `evt_` out
        // of the prose would let a lesson that merely *names* an id pass.
        let text = format!("The fix landed in {ID_A} after the retry.\n\nSee above.");
        assert!(split_citation_trailer(&text).is_none());
    }

    #[test]
    fn trailer_with_no_ids_is_none() {
        assert!(split_citation_trailer("Prose.\n\ncitations:").is_none());
        assert!(split_citation_trailer("Prose.\n\ncitations:   ").is_none());
    }

    #[test]
    fn trailer_with_a_malformed_id_is_none() {
        // Right shape, wrong id: `EventId::parse` is the single validator, so a
        // truncated ULID fails here rather than surviving into the pin pass.
        assert!(split_citation_trailer("Prose.\n\ncitations: evt_1").is_none());
        assert!(split_citation_trailer(&format!("Prose.\n\ncitations: {ID_A} nope")).is_none());
    }

    #[test]
    fn trailer_must_be_the_last_non_empty_line() {
        let text = format!("citations: {ID_A}\n\nThe prose came after.");
        assert!(
            split_citation_trailer(&text).is_none(),
            "a trailer buried above the prose is not a trailer"
        );
    }

    #[test]
    fn a_trailer_only_completion_yields_empty_prose() {
        let text = format!("citations: {ID_A}");
        let (prose, ids) = split_citation_trailer(&text).expect("parses");
        assert!(prose.is_empty(), "no prose above the trailer");
        assert_eq!(ids, vec![ID_A.to_string()]);
    }

    #[test]
    fn a_doubled_trailer_is_malformed_not_half_stripped() {
        // Stripping only the last line would leave `citations: <ID_A>` sitting in
        // `Lesson.content` — prose that was never validated and never pinned.
        let text = format!("Prose.\n\ncitations: {ID_A}\ncitations: {ID_B}");
        assert!(
            split_citation_trailer(&text).is_none(),
            "a second trailer above the last one must fall back, not leak into prose"
        );
    }

    #[test]
    fn a_citations_line_inside_the_prose_is_still_rejected() {
        // Same defect one blank line further up.
        let text = format!("Prose.\n\ncitations: {ID_A}\n\ncitations: {ID_B}");
        assert!(split_citation_trailer(&text).is_none());
    }

    #[test]
    fn prose_merely_mentioning_the_word_citations_is_fine() {
        // The guard keys on a trailing `citations:` *line*, not the word.
        let text =
            format!("We tightened citations: they must be verbatim.\nDone.\n\ncitations: {ID_A}");
        let (prose, ids) = split_citation_trailer(&text).expect("not a doubled trailer");
        assert_eq!(
            prose,
            "We tightened citations: they must be verbatim.\nDone."
        );
        assert_eq!(ids, vec![ID_A.to_string()]);
    }

    #[test]
    fn citations_are_valid_requires_a_non_empty_cluster_subset() {
        let cluster = vec![ID_A.to_string(), ID_B.to_string()];

        assert!(citations_are_valid(&[ID_A.to_string()], &cluster));
        assert!(citations_are_valid(&cluster, &cluster));

        assert!(
            !citations_are_valid(&[], &cluster),
            "an empty citation list cites nothing"
        );
        let hallucinated = "evt_01ARZ3NDEKTSV4RRFFQ69G5FZZ".to_string();
        assert!(
            !citations_are_valid(std::slice::from_ref(&hallucinated), &cluster),
            "an id outside the promoted cluster is a hallucination"
        );
        assert!(
            !citations_are_valid(&[ID_A.to_string(), hallucinated], &cluster),
            "one bad id fails the whole trailer — partial credit would pin it"
        );
        assert!(
            !citations_are_valid(&[ID_A.to_string()], &[]),
            "no cluster, nothing valid"
        );
    }

    // ── Personal layer (AILAB-199) ───────────────────────────────────────────

    /// A scratch store with a populated `personal/`. Returns the guard first so
    /// the directory outlives the root the caller reads through.
    fn personal_fixture(label: &str) -> (DirGuard, AgentRoot) {
        let dir = unique_tmpdir(label);
        let root = AgentRoot::new(&dir);
        std::fs::create_dir_all(root.personal_dir()).expect("personal/");
        (DirGuard(dir), root)
    }

    #[test]
    fn personal_context_is_none_when_the_dir_is_absent() {
        let dir = unique_tmpdir("llm-personal-absent");
        let _g = DirGuard(dir.clone());
        let root = AgentRoot::new(&dir);
        assert_eq!(load_personal_prompt_context(&root), None);
    }

    #[test]
    fn personal_context_is_none_when_every_file_is_empty() {
        let (_g, root) = personal_fixture("llm-personal-empty");
        std::fs::write(root.preferences_md(), "").unwrap();
        std::fs::write(root.decisions_md(), "   \n\t\n").unwrap();
        assert_eq!(
            load_personal_prompt_context(&root),
            None,
            "whitespace-only files must not produce a block with nothing in it"
        );
    }

    #[test]
    fn personal_context_orders_preferences_then_decisions_then_the_rest() {
        let (_g, root) = personal_fixture("llm-personal-order");
        std::fs::write(root.preferences_md(), "prefer tabs").unwrap();
        std::fs::write(root.decisions_md(), "chose axum").unwrap();
        std::fs::write(root.personal_dir().join("zebra.md"), "last").unwrap();
        std::fs::write(root.personal_dir().join("apple.md"), "third").unwrap();

        let got = load_personal_prompt_context(&root).expect("four files");
        let positions: Vec<usize> = ["PREFERENCES.md", "DECISIONS.md", "apple.md", "zebra.md"]
            .iter()
            .map(|n| got.find(n).unwrap_or_else(|| panic!("{n} in block")))
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "documented files first, then the rest by name; got:\n{got}"
        );
        assert!(got.contains("prefer tabs") && got.contains("chose axum"));
    }

    #[test]
    fn personal_context_skips_non_utf8_files_and_subdirectories() {
        let (_g, root) = personal_fixture("llm-personal-skip");
        std::fs::write(root.preferences_md(), "keep me").unwrap();
        std::fs::write(root.personal_dir().join("binary.bin"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::create_dir_all(root.personal_dir().join("nested")).unwrap();
        std::fs::write(root.personal_dir().join("nested/deep.md"), "not read").unwrap();

        let got = load_personal_prompt_context(&root).expect("one readable file");
        assert!(got.contains("keep me"));
        assert!(!got.contains("binary.bin"), "non-UTF-8 file is skipped");
        assert!(!got.contains("not read"), "the layer is one flat directory");
    }

    /// A symlink planted in `personal/` must not be able to pull an arbitrary
    /// file into a prompt — the whole point of resolving before reading.
    #[cfg(unix)]
    #[test]
    fn personal_context_refuses_a_symlink_that_escapes_the_layer() {
        let (_g, root) = personal_fixture("llm-personal-escape");
        std::fs::write(root.preferences_md(), "keep me").unwrap();

        let outside = root.project_root().join("outside-secret.md");
        std::fs::write(&outside, "SECRET-OUTSIDE-THE-PERSONAL-LAYER").unwrap();
        std::os::unix::fs::symlink(&outside, root.personal_dir().join("escape.md")).unwrap();

        let got = load_personal_prompt_context(&root).expect("the real file still loads");
        assert!(got.contains("keep me"));
        assert!(
            !got.contains("SECRET-OUTSIDE-THE-PERSONAL-LAYER"),
            "a symlink out of personal/ must not be followed; got:\n{got}"
        );
    }

    #[test]
    fn personal_context_truncates_at_the_shared_16_kib_cap() {
        let (_g, root) = personal_fixture("llm-personal-cap");
        std::fs::write(
            root.preferences_md(),
            "x".repeat(PERSONAL_LAYER_MAX_BYTES * 2),
        )
        .unwrap();

        let got = load_personal_prompt_context(&root).expect("oversized file still loads");
        assert!(
            got.len() <= PERSONAL_LAYER_MAX_BYTES,
            "an oversized personal/ must not be able to blow the AILAB-196 cap: \
             {} bytes",
            got.len()
        );
        assert_eq!(
            PERSONAL_LAYER_MAX_BYTES,
            16 * 1024,
            "the prompt cap is the same 16 KiB GET /preferences truncates at"
        );
    }

    /// Truncation must land on a char boundary — `String::truncate` panics
    /// otherwise, and prose straddling the cap is the normal case, not exotic.
    #[test]
    fn personal_context_truncation_respects_char_boundaries() {
        let (_g, root) = personal_fixture("llm-personal-utf8-cap");
        // '€' is 3 bytes and 16 KiB is not divisible by 3, so the cap falls
        // inside a character no matter where the block header ends.
        std::fs::write(root.preferences_md(), "€".repeat(PERSONAL_LAYER_MAX_BYTES)).unwrap();
        let got = load_personal_prompt_context(&root).expect("loads");
        assert!(got.len() <= PERSONAL_LAYER_MAX_BYTES);
    }
}
