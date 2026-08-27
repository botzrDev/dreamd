//! Dream cycle orchestration — single owner of the full phase graph.
//!
//! Phase order (load-bearing):
//!   1. Consolidation — cluster → lesson body → LESSONS.md → pin/unpin
//!   2. Decay — archive + JSONL prune
//!   3. Index — recurrence sidecar apply → prune decayed docs
//!   4. Autobiography — best-effort git commit
//!
//! [`run_filesystem_phases`] opens and commits the single WAL envelope that
//! spans consolidation + decay (one cycle = one atomic region).
//!
//! AILAB-204 made phase 1 async: the LLM call that composes the lesson body is
//! `await`ed **inside** that same WAL envelope, between cluster selection and the
//! `LESSONS.md` write. That is deliberate — it keeps the 409 in-progress guard
//! honest and keeps the coordinator the single writer. It also means a manual
//! cycle serializes `AppendLearning` for the duration of the model call; HTTP
//! callers see 503 + `Retry-After` rather than a lost write.
//!
//! Entry points (`dreamd dream`, `POST /api/v1/dream`) are thin adapters over
//! this module. The coordinator actor calls [`run_filesystem_phases`] only so it
//! can reopen its long-lived append fd after atomic renames (WEG-271).

use std::path::{Path, PathBuf};

use crate::autobiography::{self, AutobiographyOutcome};
use crate::consolidation::{
    self, DreamCycleError as ConsolidationError, LessonBodySource, SelectedLesson,
};
use crate::decay::{self, DecayError, DecayResult};
use crate::layout::AgentRoot;
use crate::llm::{self, LlmBackend};
use crate::wal::{self, WalError};

/// Unified error type for dream-cycle orchestration.
#[derive(Debug, thiserror::Error)]
pub enum DreamCycleError {
    #[error("consolidation: {0}")]
    Consolidation(#[from] ConsolidationError),
    #[error("decay: {0}")]
    Decay(#[from] DecayError),
    #[error("WAL: {0}")]
    Wal(#[from] WalError),
    #[cfg(unix)]
    #[error("index: {0}")]
    Index(#[from] crate::server::index_map::IndexError),
    #[error("dream cycle already in progress")]
    InProgress,
}

/// Outcome of a full dream cycle (filesystem + post phases).
#[derive(Debug)]
pub struct DreamCycleResult {
    pub decay: DecayResult,
    #[cfg(unix)]
    pub autobiography: Option<AutobiographyOutcome>,
    #[cfg(not(unix))]
    pub autobiography: Option<()>,
}

/// How post-filesystem index updates reach the Tantivy indexer task.
#[cfg(unix)]
pub enum IndexBackend {
    /// Reuse the daemon's live indexer channel (HTTP path).
    Sender(tokio::sync::mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>),
    /// Open a fresh handle for this operation (CLI in-process path).
    FreshHandle,
    /// Skip index mutations (tests, or no index wired).
    Skip,
}

/// Options for post-filesystem phases (index + autobiography).
pub struct PostPhaseOptions<'a> {
    pub agent_root: &'a AgentRoot,
    pub project_root: &'a Path,
    pub cycle_date: &'a str,
    pub decay_result: &'a DecayResult,
    pub dirty_at_cycle_start: &'a [PathBuf],
    pub commit_autobiography: bool,
    #[cfg(unix)]
    pub index: IndexBackend,
}

/// Derive `YYYY-MM-DD` from a caller-supplied unix timestamp.
#[must_use]
pub fn cycle_date_from_now_sec(now_sec: i64) -> String {
    chrono::DateTime::from_timestamp(now_sec, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .format("%Y-%m-%d")
        .to_string()
}

/// Reject concurrent cycles (HTTP 409 guard).
pub fn ensure_not_in_progress(agent_root: &AgentRoot) -> Result<(), DreamCycleError> {
    match wal::read_last_cycle_status(agent_root)? {
        status if status == "in_progress" => Err(DreamCycleError::InProgress),
        _ => Ok(()),
    }
}

/// Filesystem phases: consolidation then decay.
///
/// Opens one WAL envelope before consolidation and commits it after decay so the
/// full cycle is a single atomic region (ARCH-2). Called by the coordinator
/// actor, which must reopen its append fd afterward.
///
/// `no_llm` forces the deterministic body even when credentials exist. When it is
/// `false`, the backend is resolved from the project's [`crate::config::Config`]
/// plus [`llm::resolve_llm_credentials`]; absent credentials log
/// [`llm::NO_API_KEY_FALLBACK`] and fall through to the same deterministic path.
///
/// `share_personal` is the per-call consent from `dreamd dream --share-personal`
/// (or `x-dreamd-share-personal: 1`). It is **only** permission to read
/// `.agent/personal/`; it never causes a model call that would not otherwise
/// happen, so `no_llm = true` ignores it entirely and reads nothing (AILAB-199).
pub async fn run_filesystem_phases(
    agent_root: &AgentRoot,
    now_sec: i64,
    cycle_date: &str,
    no_llm: bool,
    share_personal: bool,
) -> Result<DecayResult, DreamCycleError> {
    if no_llm {
        // Turbofish only names the type the `None` needs; no client is built.
        // `share_personal` is deliberately dropped on this arm rather than
        // forwarded: with no backend there is no payload to consent to, and
        // reading `personal/` here would be a disclosure with no recipient.
        return filesystem_phases::<llm::GenaiBackend>(
            agent_root, now_sec, cycle_date, None, false,
        )
        .await;
    }

    // A broken config.toml must not fail a cycle that would otherwise run
    // deterministically — degrade to defaults and say so.
    let config = crate::config::load_config(agent_root.project_root()).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "config load failed; using defaults for the LLM phase");
        crate::config::Config::default()
    });

    match llm::resolve_backend(&config) {
        Some(backend) => {
            filesystem_phases(
                agent_root,
                now_sec,
                cycle_date,
                Some((&backend, config.model.as_str())),
                share_personal,
            )
            .await
        }
        None => {
            filesystem_phases::<llm::GenaiBackend>(agent_root, now_sec, cycle_date, None, false)
                .await
        }
    }
}

/// [`run_filesystem_phases`] with an explicit backend — the seam tests use to
/// drive the LLM path with a fake and no network.
///
/// `share_personal` is `false` for every pre-AILAB-199 caller; the flag exists
/// here so a test can drive the consent path against the same fake backend the
/// rest of the LLM assertions use.
pub async fn run_filesystem_phases_with_backend<B: LlmBackend>(
    agent_root: &AgentRoot,
    now_sec: i64,
    cycle_date: &str,
    backend: &B,
    model: &str,
    share_personal: bool,
) -> Result<DecayResult, DreamCycleError> {
    filesystem_phases(
        agent_root,
        now_sec,
        cycle_date,
        Some((backend, model)),
        share_personal,
    )
    .await
}

/// The one WAL envelope. `llm` is `None` for every deterministic trigger
/// (`--no-llm`, no credentials, or a test that wants today's bytes).
async fn filesystem_phases<B: LlmBackend>(
    agent_root: &AgentRoot,
    now_sec: i64,
    cycle_date: &str,
    llm_target: Option<(&B, &str)>,
    share_personal: bool,
) -> Result<DecayResult, DreamCycleError> {
    // ARCH-2: ONE WAL envelope spans BOTH filesystem phases. begin_cycle guards
    // `.agent/` existence (WEG-281) and sets state=in_progress; commit_cycle sets
    // state=complete and deletes the WAL. A phase error short-circuits via `?`
    // BEFORE commit, leaving an uncommitted WAL for next-startup recovery
    // (state=failed) — so no half-finished cycle is ever recorded "complete".
    wal::begin_cycle(agent_root, now_sec)?;

    // Consolidation, split so the model call can sit between selection and the
    // write while both still share one exemplar id, one WAL intent, one pin pass.
    // `None` = nothing promoted; `select_lesson_or_retire` already unlinked any
    // stale LESSONS.md (AILAB-699) and decay still runs below.
    if let Some(selected) = consolidation::select_lesson_or_retire(agent_root, now_sec)? {
        // The spend cap (AILAB-196) is read here rather than threaded through
        // `run_filesystem_phases_with_backend`, because that signature is the
        // FakeLlm seam and its callers hold a backend, not a project config.
        // Reading it inside this arm also keeps a cycle that promotes nothing
        // free of the extra file read. Same degrade-to-defaults rule as the
        // production entry point: a broken config.toml must never fail a cycle.
        let cap_usd = crate::config::load_config(agent_root.project_root())
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "config load failed; using the default LLM cost cap");
                crate::config::Config::default()
            })
            .cost_cap_usd;
        let body =
            compose_lesson_body(&selected, llm_target, cap_usd, agent_root, share_personal).await;
        consolidation::write_selected_lesson(agent_root, now_sec, &selected, body)?;
    }

    let decay = decay::run_decay_pruner(agent_root, now_sec, cycle_date)?;
    wal::commit_cycle(agent_root, now_sec)?;
    Ok(decay)
}

/// Compose the lesson body. **Infallible by construction** — every LLM failure
/// mode resolves to [`LessonBodySource::Deterministic`], so a model outage can
/// never abort a cycle or leave `LESSONS.md` unwritten.
///
/// `cap_usd` is the caller's [`crate::config::Config::cost_cap_usd`] (AILAB-196).
/// A spend limit can only be enforced in the moment before the connection opens
/// — there is no refund arm after a completion — so the estimate is a gate here
/// rather than a reconciliation later.
///
/// `share_personal` is the per-call consent gate for `.agent/personal/`
/// (AILAB-199). This is the **only** place in the tree that turns that flag into
/// a read, which is what makes "default excludes the personal layer" a property
/// of one branch rather than a convention several call sites have to keep.
/// Ordering inside this function is load-bearing twice over: the personal bytes
/// are appended *after* `build_lesson_prompt` (so the cluster prompt stays
/// cluster-only and can never carry them by construction) and *before*
/// [`llm::estimate_prompt_cost`] (so the AILAB-196 cap prices the string that is
/// actually about to be sent, not a shorter one).
async fn compose_lesson_body<B: LlmBackend>(
    selected: &SelectedLesson,
    llm_target: Option<(&B, &str)>,
    cap_usd: f64,
    agent_root: &AgentRoot,
    share_personal: bool,
) -> LessonBodySource {
    let Some((backend, model)) = llm_target else {
        // No backend, no request, nothing to consent to — `personal/` is not
        // read on this arm even with the flag set. `--no-llm --share-personal`
        // is a legal combination that discloses nothing.
        return LessonBodySource::Deterministic;
    };

    let mut prompt = llm::build_lesson_prompt(&selected.cluster_key, &selected.events);

    // One line, one gate: consent is the *only* thing that can produce a read of
    // `personal/`, and keeping the flag and the call in one expression is what
    // lets a grep prove that without reading the surrounding block. The
    // `.flatten()` sits on the consumer instead: outer `None` is "not
    // consented", inner `None` is "consented, nothing to attach".
    let personal = share_personal.then(|| llm::load_personal_prompt_context(agent_root));
    if let Some(context) = personal.flatten() {
        // WARN on the cycle that actually attaches bytes, not on daemon boot:
        // an operator grepping their log for a disclosure wants one line per
        // disclosure, and a startup banner that fires when nothing was shared
        // would train them to ignore exactly this line.
        tracing::warn!(
            bytes = context.len(),
            cluster_key = %selected.cluster_key,
            "share-personal is set; attaching personal/ context to the composition prompt"
        );
        prompt.push_str("\n\n");
        prompt.push_str(llm::PERSONAL_CONTEXT_HEADING);
        prompt.push('\n');
        prompt.push_str(&context);
    }

    // Cost gate (AILAB-196), deliberately positioned between the prompt and the
    // first attempt: it needs the exact bytes that are about to be sent, and
    // estimating any deeper — inside the backend — would both re-count on every
    // retry and arrive after the request it was supposed to prevent.
    let Some(estimate) = llm::estimate_prompt_cost(model, &prompt) else {
        // No published rate means no bound on what this call costs. Pricing the
        // unknown at zero would make a typo'd `model` string the one way to run
        // uncapped, so an unpriced model is refused like any breach.
        tracing::warn!(
            model = %model,
            cap_usd,
            cluster_key = %selected.cluster_key,
            "no published rate for this model, so its cost estimate exceeds cap by \
             default; writing the deterministic body"
        );
        return LessonBodySource::Deterministic;
    };
    if llm::exceeds_cap(&estimate, cap_usd) {
        // `input_tokens` rides along because the two causes of a breach have
        // different fixes: an oversized cluster is not a repriced model.
        tracing::warn!(
            model = %model,
            estimated_usd = estimate.estimated_usd,
            cap_usd,
            input_tokens = estimate.input_tokens,
            cluster_key = %selected.cluster_key,
            "cost estimate exceeds cap; skipping the model call and writing the \
             deterministic body"
        );
        return LessonBodySource::Deterministic;
    }

    match llm::complete_with_retries(backend, model, &prompt).await {
        // Trim only the envelope whitespace; the prose itself is the model's.
        Ok(completion) => compose_from_completion(selected, completion.text.trim()),
        Err(e) => {
            tracing::warn!(
                error = %e,
                cluster_key = %selected.cluster_key,
                "llm lesson composition failed; writing the deterministic body"
            );
            LessonBodySource::Deterministic
        }
    }
}

/// Gate one completion on its citation trailer (AILAB-200 / DR-306).
///
/// A model that cannot name the cluster events it drew on has not composed a
/// lesson from them, so an unparseable or hallucinated trailer is treated
/// exactly like a transport failure: WARN, then the deterministic exemplar copy.
/// The cycle is still infallible here — this returns a body, never an error.
fn compose_from_completion(selected: &SelectedLesson, text: &str) -> LessonBodySource {
    // AILAB-204's empty-body rule runs first, so an empty answer is reported as
    // an empty answer rather than as a missing trailer.
    if text.is_empty() {
        return LessonBodySource::Deterministic;
    }

    let Some((prose, citations)) = llm::split_citation_trailer(text) else {
        tracing::warn!(
            cluster_key = %selected.cluster_key,
            "llm lesson had no parseable `citations:` trailer; writing the deterministic body"
        );
        return LessonBodySource::Deterministic;
    };

    // A trailer with nothing above it is the 204 empty-body case reached through
    // a new door, so it degrades before the citation check like any empty answer
    // — but it is worth a line: silently writing the exemplar copy is otherwise
    // indistinguishable from the model never having been called.
    if prose.is_empty() {
        tracing::warn!(
            cluster_key = %selected.cluster_key,
            cited = ?citations,
            "llm lesson was a citations trailer with no prose; writing the deterministic body"
        );
        return LessonBodySource::Deterministic;
    }

    let cluster_ids: Vec<String> = selected
        .events
        .iter()
        .map(|e| e.id.as_str().to_string())
        .collect();
    if !llm::citations_are_valid(&citations, &cluster_ids) {
        // Log the offenders, not the whole list: the operator needs to know the
        // model invented ids, and which.
        let unknown: Vec<&String> = citations
            .iter()
            .filter(|id| !cluster_ids.iter().any(|c| c == *id))
            .collect();
        tracing::warn!(
            cluster_key = %selected.cluster_key,
            cited = ?citations,
            unknown = ?unknown,
            "llm lesson cited ids outside the promoted cluster; writing the deterministic body"
        );
        return LessonBodySource::Deterministic;
    }

    LessonBodySource::Llm {
        content: prose,
        prompt_version: llm::VERSIONED_PROMPT_ID.to_string(),
        citations,
    }
}

/// Index + autobiography phases. Async because Tantivy ops run on the indexer task.
#[cfg(unix)]
pub async fn run_post_phases(
    opts: PostPhaseOptions<'_>,
) -> Result<Option<AutobiographyOutcome>, DreamCycleError> {
    run_index_phases(opts.agent_root, opts.decay_result, opts.index).await?;

    if !opts.commit_autobiography {
        return Ok(None);
    }

    match autobiography::commit_cycle(opts.agent_root, opts.cycle_date, opts.dirty_at_cycle_start) {
        Ok(outcome) => Ok(Some(outcome)),
        Err(e) => {
            tracing::error!(
                error = %e,
                "autobiography commit failed (dream cycle still succeeded)"
            );
            Ok(None)
        }
    }
}

#[cfg(not(unix))]
pub async fn run_post_phases(opts: PostPhaseOptions<'_>) -> Result<Option<()>, DreamCycleError> {
    let _ = opts;
    Ok(None)
}

/// Full in-process cycle for the CLI (`--no-commit` or no daemon).
///
/// ONE current-thread runtime drives the whole cycle. Since AILAB-204 the
/// filesystem phases are async too (they `await` the model call), so they share
/// the runtime that already existed for the index phases — building a second one
/// inside the first would panic at the nested `block_on`.
pub fn run_in_process(
    project_root: &Path,
    now_sec: i64,
    no_commit: bool,
    no_llm: bool,
    share_personal: bool,
    dirty_at_cycle_start: Vec<PathBuf>,
) -> Result<DreamCycleResult, DreamCycleError> {
    let agent_root = AgentRoot::new(project_root);
    let cycle_date = cycle_date_from_now_sec(now_sec);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime for dream cycle");

    runtime.block_on(async {
        let decay =
            run_filesystem_phases(&agent_root, now_sec, &cycle_date, no_llm, share_personal)
                .await?;

        #[cfg(unix)]
        {
            let autobiography = run_post_phases(PostPhaseOptions {
                agent_root: &agent_root,
                project_root,
                cycle_date: &cycle_date,
                decay_result: &decay,
                dirty_at_cycle_start: &dirty_at_cycle_start,
                commit_autobiography: !no_commit,
                index: IndexBackend::FreshHandle,
            })
            .await?;

            Ok(DreamCycleResult {
                decay,
                autobiography,
            })
        }

        #[cfg(not(unix))]
        {
            let _ = (no_commit, &dirty_at_cycle_start, project_root);
            Ok(DreamCycleResult {
                decay,
                autobiography: None,
            })
        }
    })
}

/// What `dreamd dream --dry` would have persisted, built without *writing*
/// anything (AILAB-341). Reads: the episodic JSONL always, plus `config.toml`
/// and the credential files when the LLM arm is live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryPreview {
    /// `None` = would retire LESSONS.md (AILAB-699). File is not touched.
    pub lessons_markdown: Option<String>,
}

/// Preview one dream cycle's consolidation phase and return the `LESSONS.md`
/// bytes it would write — writing nothing.
///
/// Read-only twin of [`run_in_process`], and the whole of `dreamd dream --dry`.
/// It reads the episodic JSONL, clusters in memory, composes a body, and renders
/// through the same `lessons::render_lessons_file` the writer serializes with —
/// so for any given body the preview is byte-identical to the file a real cycle
/// would persist. Under `no_llm` (and on any LLM fallback) that makes the whole
/// preview reproducible; with a live model the prose is the model's, so a later
/// real cycle composes its own and the bytes will differ.
///
/// Deliberately absent: the WAL envelope, the recurrence sidecar, the retire
/// unlink, the JSONL pin rewrite, decay, Tantivy, and the autobiography commit.
/// Every one of those is a write, and `--dry` has none. `--no-commit` is
/// therefore meaningless here and is ignored by the caller.
///
/// The model **is** allowed to run when `no_llm` is false and credentials
/// resolve — comparing composed prose against the raw log is the point of the
/// flag. Composition stays infallible: any LLM failure degrades to the
/// deterministic exemplar copy, exactly as on the write path.
///
/// `share_personal` is honored here too (AILAB-199). `--dry` skips the daemon
/// proxy, so the consent cannot arrive as a header on this path and has to be
/// taken in-process — and a preview that silently dropped it would show the
/// operator a prompt the real cycle is not going to send.
///
/// Uses the same current-thread runtime shape as [`run_in_process`] so the body
/// composition can `await`; building a second runtime inside an existing one
/// would panic at the nested `block_on`.
pub fn preview_in_process(
    project_root: &Path,
    now_sec: i64,
    no_llm: bool,
    share_personal: bool,
) -> Result<DryPreview, DreamCycleError> {
    let agent_root = AgentRoot::new(project_root);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime for dream preview");

    runtime.block_on(preview_phases(&agent_root, now_sec, no_llm, share_personal))
}

/// Async half of [`preview_in_process`]: select, compose, render.
async fn preview_phases(
    agent_root: &AgentRoot,
    now_sec: i64,
    no_llm: bool,
    share_personal: bool,
) -> Result<DryPreview, DreamCycleError> {
    let Some(selected) = consolidation::preview_select_lesson(agent_root, now_sec)? else {
        // Nothing promotes. The write path would unlink a stale LESSONS.md here;
        // a preview reports the intent and leaves the file alone.
        return Ok(DryPreview {
            lessons_markdown: None,
        });
    };

    let body = if no_llm {
        // Turbofish only names the type the `None` needs; no client is built.
        // The cap is never consulted on this arm — `None` returns deterministic
        // before the estimate — so `--dry --no-llm` still reads no config.
        compose_lesson_body::<llm::GenaiBackend>(
            &selected,
            None,
            crate::config::Config::default().cost_cap_usd,
            agent_root,
            false,
        )
        .await
    } else {
        // A broken config.toml must not fail a preview that would otherwise
        // render deterministically — degrade to defaults and say so.
        let config = crate::config::load_config(agent_root.project_root()).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "config load failed; using defaults for the preview LLM phase");
            crate::config::Config::default()
        });
        match llm::resolve_backend(&config) {
            // The preview spends real money on the same terms as the write path,
            // so it is gated by the same `cost_cap_usd` — an over-cap `--dry`
            // renders the deterministic body it is previewing.
            Some(backend) => {
                compose_lesson_body(
                    &selected,
                    Some((&backend, config.model.as_str())),
                    config.cost_cap_usd,
                    agent_root,
                    share_personal,
                )
                .await
            }
            None => {
                compose_lesson_body::<llm::GenaiBackend>(
                    &selected,
                    None,
                    config.cost_cap_usd,
                    agent_root,
                    false,
                )
                .await
            }
        }
    };

    let lessons_file = consolidation::lessons_file_from_selected(now_sec, &selected, body);
    Ok(DryPreview {
        lessons_markdown: Some(crate::lessons::render_lessons_file(&lessons_file)),
    })
}

#[cfg(unix)]
async fn run_index_phases(
    agent_root: &AgentRoot,
    decay_result: &DecayResult,
    index: IndexBackend,
) -> Result<(), DreamCycleError> {
    match index {
        IndexBackend::Skip => Ok(()),
        IndexBackend::Sender(sender) => {
            apply_recurrence_sidecar_if_present(agent_root, &sender).await?;
            if !decay_result.decayed_ids.is_empty() {
                prune_decayed_events(&sender, decay_result.decayed_ids.clone()).await?;
            }
            // DR-211: consolidation rewrote LESSONS.md earlier in this cycle;
            // fold the new lesson set into the live index without a restart.
            index_semantic_lessons(agent_root, &sender).await?;
            Ok(())
        }
        IndexBackend::FreshHandle => {
            use crate::server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};

            let handle = TantivyIndexHandle::open(agent_root, DEFAULT_COMMIT_CADENCE)?;
            let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
            if sidecar_path.exists() {
                handle.apply_recurrence_sidecar(agent_root).await?;
            }
            if !decay_result.decayed_ids.is_empty() {
                handle
                    .prune_decayed_events(decay_result.decayed_ids.clone())
                    .await?;
            }
            // `open()` above already ran the semantic pass; re-running it after
            // the sidecar and prune commits is idempotent (delete every semantic
            // document, re-add the current lesson set).
            handle.index_semantic_lessons(agent_root.clone()).await?;
            Ok(())
        }
    }
}

#[cfg(unix)]
async fn apply_recurrence_sidecar_if_present(
    agent_root: &AgentRoot,
    sender: &tokio::sync::mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>,
) -> Result<(), DreamCycleError> {
    use crate::server::tantivy_handle::IndexerMsg;
    use tokio::sync::oneshot;

    let sidecar_path = agent_root.semantic_dir().join("recurrence_counts.json");
    if !sidecar_path.exists() {
        return Ok(());
    }

    let (tx, rx) = oneshot::channel();
    sender
        .send(IndexerMsg::ApplyRecurrenceSidecar {
            agent_root: agent_root.clone(),
            response: tx,
        })
        .await
        .map_err(|_| crate::server::index_map::IndexError::ChannelClosed)?;
    rx.await
        .map_err(|_| crate::server::index_map::IndexError::TaskDropped)??;
    Ok(())
}

/// Ask the live indexer to re-index `semantic/LESSONS.md` (DR-211 / AILAB-205).
/// Sender-only mirror of [`TantivyIndexHandle::index_semantic_lessons`] for the
/// daemon path, which holds a channel rather than the handle.
///
/// Unconditional: unlike the recurrence sidecar there is no "file present"
/// pre-check, because the pass must also run to clear stale semantic documents
/// after a cycle that removed LESSONS.md.
///
/// [`TantivyIndexHandle::index_semantic_lessons`]: crate::server::TantivyIndexHandle::index_semantic_lessons
#[cfg(unix)]
async fn index_semantic_lessons(
    agent_root: &AgentRoot,
    sender: &tokio::sync::mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>,
) -> Result<(), DreamCycleError> {
    use crate::server::tantivy_handle::IndexerMsg;
    use tokio::sync::oneshot;

    let (tx, rx) = oneshot::channel();
    sender
        .send(IndexerMsg::IndexSemanticLessons {
            agent_root: agent_root.clone(),
            response: tx,
        })
        .await
        .map_err(|_| crate::server::index_map::IndexError::ChannelClosed)?;
    rx.await
        .map_err(|_| crate::server::index_map::IndexError::TaskDropped)??;
    Ok(())
}

#[cfg(unix)]
async fn prune_decayed_events(
    sender: &tokio::sync::mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>,
    event_ids: Vec<dreamd_protocol::EventId>,
) -> Result<(), DreamCycleError> {
    use crate::server::tantivy_handle::IndexerMsg;
    use tokio::sync::oneshot;

    let (tx, rx) = oneshot::channel();
    sender
        .send(IndexerMsg::PruneDecayedEvents {
            event_ids,
            response: tx,
        })
        .await
        .map_err(|_| crate::server::index_map::IndexError::ChannelClosed)?;
    rx.await
        .map_err(|_| crate::server::index_map::IndexError::TaskDropped)??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::Ordering;

    const NOW_SEC: i64 = 1_747_137_600;

    fn fixture_jsonl() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/dream-cycle-snapshot/AGENT_LEARNINGS.jsonl")
    }

    fn scaffold_fixture() -> (tempfile::TempDir, AgentRoot) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = AgentRoot::new(dir.path());
        let episodic_dir = root.episodic_jsonl().parent().unwrap().to_owned();
        fs::create_dir_all(&episodic_dir).unwrap();
        fs::copy(fixture_jsonl(), root.episodic_jsonl()).expect("copy fixture");
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        (dir, root)
    }

    /// Deterministic backend for every pre-AILAB-204 assertion: `no_llm = true`
    /// keeps these tests network-free and byte-identical to the old behavior
    /// regardless of whether the developer running them has an API key exported.
    async fn run_deterministic(root: &AgentRoot) -> DecayResult {
        run_filesystem_phases(
            root,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            true,
            false,
        )
        .await
        .expect("filesystem phases")
    }

    #[tokio::test]
    async fn filesystem_phases_produces_lessons_on_fixture() {
        let (_dir, root) = scaffold_fixture();

        run_deterministic(&root).await;

        assert!(root.lessons_md().exists(), "LESSONS.md must exist");
        let sidecar = root.semantic_dir().join("recurrence_counts.json");
        assert!(sidecar.exists(), "recurrence_counts.json must exist");
    }

    #[test]
    fn in_process_full_cycle_on_fixture() {
        let (_dir, root) = scaffold_fixture();
        let project_root = root.project_root().to_path_buf();

        let result = run_in_process(&project_root, NOW_SEC, true, true, false, Vec::new())
            .expect("full in-process cycle");

        assert!(root.lessons_md().exists());
        assert!(!result.decay.decayed_ids.is_empty() || result.decay.kept_count > 0);
    }

    #[tokio::test]
    async fn single_wal_envelope_committed_and_cleaned_after_full_cycle() {
        let (_dir, root) = scaffold_fixture();
        run_deterministic(&root).await;
        assert!(
            !root.wal_path().exists(),
            "single envelope committed + WAL cleaned"
        );
        assert_eq!(
            crate::wal::read_last_cycle_status(&root).unwrap(),
            "complete",
            "exactly one transition to complete, at the end"
        );
    }

    // ── AILAB-341: `dreamd dream --dry` preview ──────────────────────────────

    /// The preview's whole claim is "this is what the cycle would write". Pin it
    /// against the real thing: preview first (asserting the store is untouched),
    /// then run a deterministic cycle over the same fixture at the same
    /// `now_sec` and compare the bytes.
    #[test]
    fn preview_is_byte_identical_to_the_no_llm_cycle_it_previews() {
        let (_dir, root) = scaffold_fixture();
        let project_root = root.project_root().to_path_buf();
        let sidecar = root.semantic_dir().join("recurrence_counts.json");
        let jsonl_before = fs::read(root.episodic_jsonl()).unwrap();

        let preview = preview_in_process(&project_root, NOW_SEC, true, false).expect("preview");
        let markdown = preview
            .lessons_markdown
            .expect("the fixture promotes a cluster");

        assert!(
            !root.lessons_md().exists(),
            "a dry run must not create LESSONS.md"
        );
        assert!(!sidecar.exists(), "a dry run must not write the sidecar");
        assert!(!root.wal_path().exists(), "a dry run must not open a WAL");
        assert_eq!(
            fs::read(root.episodic_jsonl()).unwrap(),
            jsonl_before,
            "a dry run must not rewrite the episodic log"
        );

        run_in_process(&project_root, NOW_SEC, true, true, false, Vec::new()).expect("real cycle");

        assert_eq!(
            markdown,
            fs::read_to_string(root.lessons_md()).unwrap(),
            "the preview must be byte-identical to the LESSONS.md the cycle wrote"
        );
    }

    /// Nothing promotes: the preview reports "would retire" (`None`) and leaves
    /// the stale `LESSONS.md` on disk. The write path unlinks it (AILAB-699) —
    /// that difference is the point of the preview.
    #[test]
    fn preview_without_promotion_reports_none_and_leaves_lessons_md() {
        let (_dir, root) = scaffold_fixture();
        let project_root = root.project_root().to_path_buf();

        fs::create_dir_all(root.semantic_dir()).unwrap();
        let existing = crate::lessons::LessonsFile {
            last_updated: chrono::DateTime::from_timestamp(NOW_SEC, 0).unwrap(),
            prompt_version: "deterministic-only".to_string(),
            cluster_key: "rust::types".to_string(),
            citations: vec![],
            lessons: vec![crate::lessons::Lesson {
                id: "evt_00000000000000000000000000".to_string(),
                content: "stale lesson".to_string(),
                pinned: false,
            }],
        };
        crate::lessons::write_lessons_file(&root.lessons_md(), &existing).unwrap();
        let before = fs::read(root.lessons_md()).unwrap();

        // Past both recurrence windows, so no fixture cluster still promotes.
        // Derived from the fixture's own newest event, not from `NOW_SEC`: the
        // snapshot corpus is timestamped a year *ahead* of `NOW_SEC`, so a
        // `NOW_SEC + 30d` offset would still sit inside the window.
        let newest = crate::episodic::read_all(&root.episodic_jsonl())
            .unwrap()
            .iter()
            .map(|e| e.timestamp.timestamp())
            .max()
            .expect("fixture is non-empty");
        let later = newest + consolidation::WINDOW_30_DAYS_SEC + 1;
        let preview = preview_in_process(&project_root, later, true, false).expect("preview");

        assert!(
            preview.lessons_markdown.is_none(),
            "no promotion previews as a retire, not as an empty file"
        );
        assert_eq!(
            fs::read(root.lessons_md()).unwrap(),
            before,
            "the preview must leave the stale LESSONS.md byte-identical"
        );
        assert!(
            !root.semantic_dir().join("recurrence_counts.json").exists(),
            "the preview must not write the sidecar"
        );
        assert!(!root.wal_path().exists(), "the preview must not open a WAL");
    }

    // ── AILAB-204: LLM body composition ──────────────────────────────────────

    /// Read the single lesson the cycle wrote, with its frontmatter.
    fn read_only_lesson(root: &AgentRoot) -> (crate::lessons::LessonsFile, crate::lessons::Lesson) {
        let file = crate::lessons::read_lessons_file(&root.lessons_md()).expect("LESSONS.md reads");
        let lesson = file.lessons.first().cloned().expect("one lesson");
        (file, lesson)
    }

    /// The exemplar the deterministic path would have picked, for comparison.
    fn deterministic_exemplar(root: &AgentRoot) -> consolidation::SelectedLesson {
        consolidation::select_lesson_or_retire(root, NOW_SEC)
            .expect("cluster engine")
            .expect("fixture promotes a cluster")
    }

    /// The model's prose, minus the trailer the AILAB-200 gate strips.
    const COMPOSED: &str = "Return `?` from fallible handlers; \
            the rejected alternative was `.unwrap()`, which panicked the request task.";

    /// Ids of the promoted cluster, exemplar first, as the gate sees them.
    fn cluster_ids(root: &AgentRoot) -> Vec<String> {
        deterministic_exemplar(root)
            .events
            .iter()
            .map(|e| e.id.as_str().to_string())
            .collect()
    }

    #[tokio::test]
    async fn llm_success_writes_model_text_but_keeps_the_exemplar_id() {
        let (_dir, root) = scaffold_fixture();
        let selected = deterministic_exemplar(&root);
        let expected_id = selected.exemplar_id.clone();
        // Cite the exemplar plus one other cluster member, so the pin union has
        // something to prove.
        let ids = cluster_ids(&root);
        let other = ids
            .iter()
            .find(|id| **id != expected_id)
            .expect("fixture cluster has more than one event")
            .clone();

        let fake = crate::llm::fake::FakeLlm::ok(&format!(
            "{COMPOSED}\n\ncitations: {expected_id} {other}"
        ));

        // This also pins the happy side of the AILAB-196 gate: the fixture store
        // has no config.toml, so the cycle runs under the default $0.10 cap and a
        // real estimate of this prompt has to clear it for the LLM body to land.
        run_filesystem_phases_with_backend(
            &root,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            &fake,
            "claude-haiku-4-5",
            // AILAB-199: no consent — the default for every pre-199 assertion.
            false,
        )
        .await
        .expect("llm cycle");

        let (file, lesson) = read_only_lesson(&root);
        assert_eq!(
            lesson.content, COMPOSED,
            "body must be the model's prose with the citations trailer stripped"
        );
        assert_eq!(
            file.prompt_version,
            crate::llm::VERSIONED_PROMPT_ID,
            "frontmatter must name the versioned prompt file that composed the body"
        );
        assert_eq!(
            file.prompt_version, "dream-cycle/v1.2@2026-08-23",
            "the trailer rule moved the prompt id; frontmatter must say so"
        );
        assert_eq!(
            file.citations,
            vec![expected_id.clone(), other.clone()],
            "frontmatter carries the validated trailer, in the model's order"
        );
        assert_eq!(
            lesson.id, expected_id,
            "Lesson.id stays the exemplar evt_ id — semantic indexing inherits \
             pain/importance from it, so a minted id would break recall ranking"
        );

        // AILAB-200 pin union, end to end: the cited non-exemplar member is
        // pinned even though it is not `Lesson.id`.
        let events = crate::episodic::read_all(&root.episodic_jsonl()).expect("jsonl reads");
        let pinned = |id: &str| {
            events
                .iter()
                .find(|e| e.id.as_str() == id)
                .unwrap_or_else(|| panic!("{id} must survive the cycle"))
                .pinned
        };
        assert!(pinned(&expected_id), "exemplar is pinned as before");
        assert!(
            pinned(&other),
            "a cited cluster member must be pinned through the frontmatter union"
        );
    }

    // ── AILAB-196: pre-call cost cap ─────────────────────────────────────────

    #[tokio::test]
    async fn zero_cap_composes_deterministically_without_calling_the_model() {
        let (_dir, root) = scaffold_fixture();
        let selected = deterministic_exemplar(&root);
        let ids = cluster_ids(&root);
        // A completion the citation gate would happily accept, so the only thing
        // that can keep it out of the body is the cap itself.
        let fake = crate::llm::fake::FakeLlm::ok(&format!("{COMPOSED}\n\ncitations: {}", ids[0]));

        let body = compose_lesson_body(
            &selected,
            Some((&fake, "claude-haiku-4-5")),
            0.0,
            &root,
            false,
        )
        .await;

        assert!(
            matches!(body, LessonBodySource::Deterministic),
            "cost_cap_usd = 0.0 is how an operator says \"never call the model\"; \
             reading it as \"no limit\" would invert the one setting whose failure \
             mode is a bill"
        );
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            0,
            "the cap has to abort BEFORE the request — a refusal logged after the \
             connection opened has already spent what it was meant to save"
        );
    }

    #[tokio::test]
    async fn unpriced_model_composes_deterministically_without_calling_the_model() {
        let (_dir, root) = scaffold_fixture();
        let selected = deterministic_exemplar(&root);
        let ids = cluster_ids(&root);
        let fake = crate::llm::fake::FakeLlm::ok(&format!("{COMPOSED}\n\ncitations: {}", ids[0]));

        // Generous cap, unknown rate: the cap is not what refuses this call, the
        // missing price is. Nothing bounds a call whose rate this build has never
        // been told.
        let body = compose_lesson_body(
            &selected,
            Some((&fake, "totally-unpriced-model")),
            100.0,
            &root,
            false,
        )
        .await;

        assert!(
            matches!(body, LessonBodySource::Deterministic),
            "an unpriced model must abort to the deterministic body; treating an \
             unknown rate as $0 would make a typo'd model string the one way to \
             run uncapped"
        );
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            0,
            "an unpriced model must never reach the backend, however generous the cap"
        );
    }

    #[tokio::test]
    async fn hallucinated_citation_falls_back_to_the_deterministic_bytes() {
        let (_dir_a, root_a) = scaffold_fixture();
        run_deterministic(&root_a).await;
        let deterministic_bytes = fs::read(root_a.lessons_md()).expect("deterministic LESSONS.md");

        let (_dir_b, root_b) = scaffold_fixture();
        // Well-formed id, right shape, not in the promoted cluster.
        let fake = crate::llm::fake::FakeLlm::ok(&format!(
            "{COMPOSED}\n\ncitations: evt_01ARZ3NDEKTSV4RRFFQ69G5FZZ"
        ));
        run_filesystem_phases_with_backend(
            &root_b,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            &fake,
            "claude-haiku-4-5",
            // AILAB-199: no consent — the default for every pre-199 assertion.
            false,
        )
        .await
        .expect("a hallucinated citation must NEVER fail the cycle");

        assert_eq!(
            fs::read(root_b.lessons_md()).expect("fallback LESSONS.md"),
            deterministic_bytes,
            "an invented citation is a transport failure by another name — the \
             file must be byte-identical to the deterministic cycle, citations \
             frontmatter included"
        );
        assert_eq!(
            crate::wal::read_last_cycle_status(&root_b).unwrap(),
            "complete",
            "the cycle still committed"
        );
    }

    #[tokio::test]
    async fn citation_from_another_cluster_is_still_a_hallucination() {
        // `rust::axum::error_handling` is a real cluster in the fixture, just not
        // the promoted one. Pinning it would keep an unrelated event alive.
        let (_dir_a, root_a) = scaffold_fixture();
        run_deterministic(&root_a).await;
        let deterministic_bytes = fs::read(root_a.lessons_md()).expect("deterministic LESSONS.md");

        let (_dir_b, root_b) = scaffold_fixture();
        let promoted = cluster_ids(&root_b);
        let outsider = crate::episodic::read_all(&root_b.episodic_jsonl())
            .expect("jsonl reads")
            .into_iter()
            .map(|e| e.id.as_str().to_string())
            .find(|id| !promoted.contains(id))
            .expect("fixture has a second cluster");

        let fake = crate::llm::fake::FakeLlm::ok(&format!("{COMPOSED}\n\ncitations: {outsider}"));
        run_filesystem_phases_with_backend(
            &root_b,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            &fake,
            "claude-haiku-4-5",
            // AILAB-199: no consent — the default for every pre-199 assertion.
            false,
        )
        .await
        .expect("cycle must still commit");

        assert_eq!(
            fs::read(root_b.lessons_md()).expect("fallback LESSONS.md"),
            deterministic_bytes,
            "a real id from the wrong cluster must not pass the gate"
        );
    }

    #[tokio::test]
    async fn missing_citation_trailer_falls_back_to_the_deterministic_bytes() {
        let (_dir_a, root_a) = scaffold_fixture();
        run_deterministic(&root_a).await;
        let deterministic_bytes = fs::read(root_a.lessons_md()).expect("deterministic LESSONS.md");

        let (_dir_b, root_b) = scaffold_fixture();
        // Prose only — exactly what the pre-AILAB-200 prompt asked for.
        let fake = crate::llm::fake::FakeLlm::ok(COMPOSED);
        run_filesystem_phases_with_backend(
            &root_b,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            &fake,
            "claude-haiku-4-5",
            // AILAB-199: no consent — the default for every pre-199 assertion.
            false,
        )
        .await
        .expect("a missing trailer must NEVER fail the cycle");

        assert_eq!(
            fs::read(root_b.lessons_md()).expect("fallback LESSONS.md"),
            deterministic_bytes,
            "no trailer is the same fallback as no completion"
        );
    }

    #[tokio::test]
    async fn ids_named_only_in_the_prose_do_not_satisfy_the_gate() {
        let (_dir_a, root_a) = scaffold_fixture();
        run_deterministic(&root_a).await;
        let deterministic_bytes = fs::read(root_a.lessons_md()).expect("deterministic LESSONS.md");

        let (_dir_b, root_b) = scaffold_fixture();
        let ids = cluster_ids(&root_b);
        // Every id is real and in the cluster — but they are in the prose, not
        // in a trailer. The gate reads the trailer line and nothing else.
        let fake =
            crate::llm::fake::FakeLlm::ok(&format!("{COMPOSED} See {} and {}.", ids[0], ids[1]));
        run_filesystem_phases_with_backend(
            &root_b,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            &fake,
            "claude-haiku-4-5",
            // AILAB-199: no consent — the default for every pre-199 assertion.
            false,
        )
        .await
        .expect("cycle must still commit");

        assert_eq!(
            fs::read(root_b.lessons_md()).expect("fallback LESSONS.md"),
            deterministic_bytes,
            "scraping evt_ ids out of the prose would defeat the gate"
        );
    }

    #[tokio::test]
    async fn llm_error_writes_the_same_bytes_as_the_deterministic_cycle() {
        let (_dir_a, root_a) = scaffold_fixture();
        run_deterministic(&root_a).await;
        let deterministic_bytes = fs::read(root_a.lessons_md()).expect("deterministic LESSONS.md");

        let (_dir_b, root_b) = scaffold_fixture();
        let fake = crate::llm::fake::FakeLlm::failing("connection reset by peer");
        run_filesystem_phases_with_backend(
            &root_b,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            &fake,
            "claude-haiku-4-5",
            // AILAB-199: no consent — the default for every pre-199 assertion.
            false,
        )
        .await
        .expect("a failing model must NEVER fail the cycle");
        let fallback_bytes = fs::read(root_b.lessons_md()).expect("fallback LESSONS.md");

        assert_eq!(
            fallback_bytes, deterministic_bytes,
            "fallback output must be byte-identical to the deterministic cycle"
        );
        assert_eq!(
            crate::wal::read_last_cycle_status(&root_b).unwrap(),
            "complete",
            "the cycle still committed"
        );
    }

    #[tokio::test]
    async fn empty_completion_falls_back_to_deterministic() {
        let (_dir_a, root_a) = scaffold_fixture();
        run_deterministic(&root_a).await;
        let deterministic_bytes = fs::read(root_a.lessons_md()).expect("deterministic LESSONS.md");

        let (_dir_b, root_b) = scaffold_fixture();
        let fake = crate::llm::fake::FakeLlm::ok("   \n\t ");
        run_filesystem_phases_with_backend(
            &root_b,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            &fake,
            "claude-haiku-4-5",
            // AILAB-199: no consent — the default for every pre-199 assertion.
            false,
        )
        .await
        .expect("empty completion must not fail the cycle");

        assert_eq!(
            fs::read(root_b.lessons_md()).unwrap(),
            deterministic_bytes,
            "an empty completion must not blank out LESSONS.md"
        );
    }

    #[tokio::test]
    async fn no_llm_never_calls_the_backend() {
        let (_dir, root) = scaffold_fixture();
        // `run_filesystem_phases(.., no_llm = true)` short-circuits before any
        // credential lookup or client construction; prove the deterministic
        // frontmatter lands even so.
        run_deterministic(&root).await;
        let (file, lesson) = read_only_lesson(&root);
        assert_eq!(file.prompt_version, consolidation::DETERMINISTIC_PROMPT_ID);
        assert_eq!(
            file.citations,
            vec![lesson.id.clone()],
            "the deterministic body cites exactly the exemplar it copied, so \
             --no-llm pin behavior is unchanged by AILAB-200"
        );
    }

    // ── AILAB-199: personal-layer consent ────────────────────────────────────

    /// Unique enough that a match cannot be coincidence, and shaped so a
    /// substring search over the whole prompt is a sound test.
    const PERSONAL_CANARY: &str = "AILAB199-CANARY-do-not-send-this-to-a-model";

    /// Plant the canary in `personal/PREFERENCES.md`. Nothing else in the
    /// fixture writes to `personal/`, so a prompt containing this string can
    /// only have got it from the personal layer.
    fn write_personal_canary(root: &AgentRoot) {
        fs::create_dir_all(root.personal_dir()).expect("personal/ dir");
        fs::write(
            root.preferences_md(),
            format!("# Preferences\n\n{PERSONAL_CANARY}\n"),
        )
        .expect("write canary");
    }

    /// **The default-exclude proof.** A populated `personal/` plus a live model
    /// and no consent: the model is called, and what it is handed contains no
    /// personal byte.
    ///
    /// The assertion is on `FakeLlm::last_prompt` rather than on the written
    /// lesson deliberately — the fake never reads its input, so a leaking prompt
    /// and a clean one produce byte-identical `LESSONS.md`. Only the recorded
    /// request payload can tell them apart.
    #[tokio::test]
    async fn default_cycle_never_sends_personal_bytes_to_the_model() {
        let (_dir, root) = scaffold_fixture();
        write_personal_canary(&root);

        let ids = cluster_ids(&root);
        let fake = crate::llm::fake::FakeLlm::ok(&format!("{COMPOSED}\n\ncitations: {}", ids[0]));

        run_filesystem_phases_with_backend(
            &root,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            &fake,
            "claude-haiku-4-5",
            false,
        )
        .await
        .expect("llm cycle");

        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "the model must actually have been called — a test that leaks no \
             personal bytes because it made no request proves nothing"
        );
        let prompt = fake.last_prompt().expect("the backend recorded its prompt");
        assert!(
            !prompt.contains(PERSONAL_CANARY),
            "personal/ content reached the LLM request payload without \
             --share-personal; prompt was:\n{prompt}"
        );
        assert!(
            !prompt.contains(crate::llm::PERSONAL_CONTEXT_HEADING),
            "the personal block must not be present at all by default"
        );
        // The cluster half is unchanged — the exclusion is not achieved by
        // sending less of the log.
        assert!(prompt.contains(&ids[0]), "cluster events still travel");
    }

    /// The consent path: with `--share-personal` the same store's canary is in
    /// the payload, under the heading that makes it distinguishable from an
    /// event body that happened to repeat the same words.
    #[tokio::test]
    async fn share_personal_attaches_the_personal_layer_under_its_heading() {
        let (_dir, root) = scaffold_fixture();
        write_personal_canary(&root);

        let ids = cluster_ids(&root);
        let fake = crate::llm::fake::FakeLlm::ok(&format!("{COMPOSED}\n\ncitations: {}", ids[0]));

        run_filesystem_phases_with_backend(
            &root,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            &fake,
            "claude-haiku-4-5",
            true,
        )
        .await
        .expect("llm cycle");

        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let prompt = fake.last_prompt().expect("the backend recorded its prompt");
        assert!(
            prompt.contains(crate::llm::PERSONAL_CONTEXT_HEADING),
            "opted-in personal bytes must be introduced by their heading; got:\n{prompt}"
        );
        assert!(
            prompt.contains(PERSONAL_CANARY),
            "--share-personal must actually attach personal/; got:\n{prompt}"
        );
        assert!(
            prompt.contains("PREFERENCES.md"),
            "each attached file is named, so a reader of the prompt can tell \
             which personal file a line came from"
        );
        // The heading precedes the canary: the personal block is appended after
        // the cluster prompt, never interleaved into it.
        let heading_at = prompt
            .find(crate::llm::PERSONAL_CONTEXT_HEADING)
            .expect("heading present");
        let canary_at = prompt.find(PERSONAL_CANARY).expect("canary present");
        assert!(
            heading_at < canary_at,
            "personal bytes follow their heading"
        );
    }

    /// `--no-llm --share-personal` is legal and discloses nothing: consent to a
    /// request that is never made is not a disclosure. Zero backend calls.
    #[tokio::test]
    async fn no_llm_with_share_personal_calls_no_backend_and_stays_deterministic() {
        let (_dir, root) = scaffold_fixture();
        write_personal_canary(&root);
        let selected = deterministic_exemplar(&root);

        // `--no-llm` becomes `llm_target = None` one frame up in
        // `filesystem_phases`; a backend exists here only so "it was not called"
        // is asserted against an object that records calls.
        let fake = crate::llm::fake::FakeLlm::ok(COMPOSED);
        let body =
            compose_lesson_body::<crate::llm::fake::FakeLlm>(&selected, None, 100.0, &root, true)
                .await;
        assert!(matches!(body, LessonBodySource::Deterministic));
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            0,
            "no request may be made when there is no backend to make it to"
        );
        assert_eq!(fake.last_prompt(), None, "and no payload may be built");

        // The full cycle agrees, and the artifact carries no personal byte
        // either — `personal/` is never distilled into `semantic/`.
        run_filesystem_phases(
            &root,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            true,
            true,
        )
        .await
        .expect("deterministic cycle");
        let (file, _) = read_only_lesson(&root);
        assert_eq!(file.prompt_version, consolidation::DETERMINISTIC_PROMPT_ID);
        assert!(
            !fs::read_to_string(root.lessons_md())
                .unwrap()
                .contains(PERSONAL_CANARY),
            "personal/ must never be distilled into semantic/"
        );
    }

    /// An empty (or absent) `personal/` yields no block at all under consent —
    /// a bare heading with nothing under it would be noise in every prompt.
    #[tokio::test]
    async fn share_personal_with_an_empty_personal_dir_attaches_nothing() {
        let (_dir, root) = scaffold_fixture();
        fs::create_dir_all(root.personal_dir()).expect("personal/ dir");

        let ids = cluster_ids(&root);
        let fake = crate::llm::fake::FakeLlm::ok(&format!("{COMPOSED}\n\ncitations: {}", ids[0]));

        run_filesystem_phases_with_backend(
            &root,
            NOW_SEC,
            &cycle_date_from_now_sec(NOW_SEC),
            &fake,
            "claude-haiku-4-5",
            true,
        )
        .await
        .expect("llm cycle");

        let prompt = fake.last_prompt().expect("prompt recorded");
        assert!(
            !prompt.contains(crate::llm::PERSONAL_CONTEXT_HEADING),
            "an empty personal/ must not produce an empty block"
        );
    }

    #[test]
    fn ensure_not_in_progress_rejects_active_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        wal::begin_cycle(&root, NOW_SEC).unwrap();

        let err = ensure_not_in_progress(&root).unwrap_err();
        assert!(matches!(err, DreamCycleError::InProgress));
    }

    #[test]
    fn seam_crash_after_consolidation_recovers_not_clean() {
        // Old two-envelope bug: run_deterministic_dream_cycle committed its OWN WAL,
        // so a crash after consolidation but before decay left NO WAL → recovery
        // returned Clean and state read "complete" (a half-cycle silently recorded
        // as success). With the single hoisted envelope the orchestrator's WAL stays
        // open across the seam, so the same crash is recoverable.
        let (_dir, root) = scaffold_fixture();

        // Orchestrator opens the one envelope...
        wal::begin_cycle(&root, NOW_SEC).unwrap();
        // ...consolidation runs and records its intents but no longer commits.
        consolidation::run_deterministic_dream_cycle(&root, NOW_SEC).unwrap();

        // Simulate a crash in the seam: decay + commit never run.
        assert!(root.wal_path().exists(), "WAL must remain open in the seam");
        let wal: crate::wal::DreamWal =
            serde_json::from_str(&std::fs::read_to_string(root.wal_path()).unwrap()).unwrap();
        assert!(
            !wal.intents.contains(&crate::wal::WalIntent::Commit),
            "no Commit intent — the cycle did not finish"
        );

        // Recovery must NOT report Clean, and state must NOT be left "complete".
        let outcome = crate::wal::recover_if_needed(&root, NOW_SEC).unwrap();
        assert!(
            matches!(outcome, crate::wal::RecoveryOutcome::Recovered { .. }),
            "seam crash must be recovered, not silently Clean"
        );
        assert_eq!(crate::wal::read_last_cycle_status(&root).unwrap(), "failed");
    }
}
