//! AILAB-204 / DR-304 — LLM client wrapper with deterministic auto-fallback.
//!
//! The dream cycle composes `LESSONS.md` bodies through an [`LlmBackend`] when
//! credentials are present, and silently runs the deterministic exemplar-copy
//! path otherwise. **The cycle never fails because the model coughed**: every
//! error path here is a fallback signal, not a cycle error. Callers translate
//! `Err` / empty text into [`crate::consolidation::LessonBodySource::Deterministic`].
//!
//! ## What this module does NOT own
//!
//! * Token accounting / the `cost_cap_usd` spend cap — AILAB-196. No tokenizer
//!   crate is a dependency here; the token counts reported at INFO come from the
//!   provider's own usage block.
//! * The versioned prompt file — AILAB-201. This module never loads a prompt from
//!   disk; it carries a throwaway inline constant under [`UNVERSIONED_PROMPT_ID`]
//!   that AILAB-201 deletes.
//! * Citation validation (`evt_` ids the model made up) — AILAB-200.
//! * Personal-layer redaction of prompt inputs — AILAB-199.
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

use dreamd_protocol::AgentLearning;
use serde::Deserialize;

use crate::config::Config;

/// `prompt_version` written to `LESSONS.md` frontmatter when the body came from
/// the throwaway inline prompt below. AILAB-201 replaces this with a versioned,
/// file-backed prompt id and deletes [`UNVERSIONED_LESSON_PROMPT`].
pub const UNVERSIONED_PROMPT_ID: &str = "llm-unversioned";

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

/// Throwaway instruction block. Shape is dictated by `assignments/14-day-blitz.md`
/// §"Lesson shape": rule first, one rejected alternative when the cluster shows
/// one, never invent `evt_` ids, at most eight sentences.
///
/// AILAB-201 replaces this with a versioned file loaded at build time and
/// deletes this constant along with [`UNVERSIONED_PROMPT_ID`].
pub const UNVERSIONED_LESSON_PROMPT: &str = "\
You are writing one durable engineering lesson for a senior engineer's memory file.

Write the lesson as a projection of the events below, not a paste of any one of them.

Rules:
- Lead with the rule. State what to do, then why, in that order.
- If the events show an approach that was tried and rejected, name that rejected
  alternative in one clause. If they do not, say nothing about alternatives.
- Cite only evt_ ids that appear verbatim in the events below. Never invent an id.
- At most 8 sentences. No headings, no bullet list, no preamble, no sign-off.
- Output only the lesson prose.";

/// Build the composition prompt for one promoted cluster.
///
/// Event bodies are included verbatim; redaction of the personal layer is
/// AILAB-199 and is not applied here.
pub fn build_lesson_prompt(cluster_key: &str, events: &[AgentLearning]) -> String {
    let mut s = String::with_capacity(UNVERSIONED_LESSON_PROMPT.len() + 256 * events.len());
    s.push_str(UNVERSIONED_LESSON_PROMPT);
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

    pub(crate) struct FakeLlm {
        text: Option<String>,
        fail_with: Option<String>,
        /// Attempt counter, so a test can assert the retry budget was honored
        /// rather than merely that the final value was right.
        pub(crate) calls: AtomicU32,
    }

    impl FakeLlm {
        /// Always answers with `text` (which may be empty/whitespace).
        pub(crate) fn ok(text: &str) -> Self {
            FakeLlm {
                text: Some(text.to_string()),
                fail_with: None,
                calls: AtomicU32::new(0),
            }
        }
        /// Always fails with a transport error — the retried variant.
        pub(crate) fn failing(msg: &str) -> Self {
            FakeLlm {
                text: None,
                fail_with: Some(msg.to_string()),
                calls: AtomicU32::new(0),
            }
        }
    }

    impl LlmBackend for FakeLlm {
        async fn complete(&self, _model: &str, _prompt: &str) -> Result<LlmCompletion, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
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

        assert!(prompt.starts_with(UNVERSIONED_LESSON_PROMPT));
        assert!(prompt.contains("Cluster: rust::error_handling"));
        assert!(prompt.contains(events[0].id.as_str()));
        assert!(prompt.contains(events[1].id.as_str()));
        assert!(prompt.contains("unwrap panicked in the handler"));
        // Shape contract from assignments/14-day-blitz.md.
        assert!(prompt.contains("Never invent an id."));
        assert!(prompt.contains("At most 8 sentences."));
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
}
