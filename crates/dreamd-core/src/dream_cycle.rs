//! Dream cycle orchestration — single owner of the full phase graph.
//!
//! Phase order (load-bearing):
//!   1. Consolidation — cluster → lesson body → LESSONS.md → pin/unpin
//!   2. Decay — archive + JSONL prune
//!   3. Index — prune decayed docs → re-index the lesson layer
//!   4. Autobiography — best-effort git commit
//!
//! A cycle's filesystem phases are three steps (BZR-1241):
//!   1. [`prepare`] reads the log once and selects the lesson;
//!   2. [`compose`] writes the lesson body, calling the model when one is
//!      configured (AILAB-204);
//!   3. [`commit`] opens the stage (status `in_progress`), takes the autosnap,
//!      plans every output of consolidation + decay
//!      ([`crate::plan::plan_cycle_with`]), stages the plan, and commits it with
//!      one redo WAL (one cycle = one atomic region).
//!
//! The model call runs **before** the stage opens, and outside the coordinator.
//! The daemon prepares and commits through the coordinator actor
//! (`PrepareCycle` / `CommitCycle`) and composes in the request's own task, so
//! appends keep landing during the model call and the commit includes them.
//! [`gate_at_commit`] re-checks the composed body against what the commit
//! selects.
//!
//! Entry points (`dreamd dream`, `POST /api/v1/dream`) are thin adapters over
//! [`run_guarded_cycle`], the one sequencer (BZR-172). The coordinator actor
//! runs the commit itself so it can reopen its long-lived append fd after
//! atomic renames (WEG-271).

use std::path::{Path, PathBuf};

/// Request header that forces the daemon's cycle into deterministic mode
/// (AILAB-204). Sent as the literal `"1"` and only when `--no-llm` is set.
pub const NO_LLM_HEADER: &str = "x-dreamd-no-llm";

/// The only value [`NO_LLM_HEADER`] is honored at. Case-sensitive.
pub const NO_LLM_HEADER_VALUE: &str = "1";

/// Request header carrying per-call consent to include `.agent/personal/` in
/// the dream-cycle composition prompt (AILAB-199).
pub const SHARE_PERSONAL_HEADER: &str = "x-dreamd-share-personal";

/// The only value [`SHARE_PERSONAL_HEADER`] is honored at. Case-sensitive.
pub const SHARE_PERSONAL_HEADER_VALUE: &str = "1";
use std::time::{SystemTime, UNIX_EPOCH};

use crate::autobiography::{self, AutobiographyOutcome};
use crate::consolidation::{self, LessonBodySource, LessonPhaseError, SelectedLesson};
use crate::decay::DecayResult;
use crate::episodic;
use crate::layout::AgentRoot;
use crate::llm::{self, LlmBackend};
use crate::plan;
use crate::provenance::{Ledger, ProvenanceError};
use crate::retention;
use crate::snapshot::{self, SnapshotError};
use crate::wal::{self, CycleKind, WalError};

/// Unified error type for dream-cycle orchestration.
#[derive(Debug, thiserror::Error)]
pub enum DreamCycleError {
    #[error("consolidation: {0}")]
    Consolidation(#[from] LessonPhaseError),
    #[error("WAL: {0}")]
    Wal(#[from] WalError),
    #[error("provenance: {0}")]
    Provenance(#[from] ProvenanceError),
    #[error("snapshot: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error("index: {0}")]
    Index(#[from] crate::server::index_map::IndexError),
    #[error("dream cycle already in progress")]
    InProgress,
    /// The coordinator's inbox was full when the cycle was dispatched (HTTP 503).
    #[error("coordinator busy, retry")]
    CoordinatorBusy,
    /// The coordinator could not run the filesystem phases (closed inbox,
    /// dropped reply, or an error from the actor). The string is the message.
    #[error("{0}")]
    Coordinator(String),
    /// The in-process cycle could not take the project writer lock: another
    /// process writes this project, or recovery under the lock failed.
    #[error("{0}")]
    WriterLock(#[from] crate::writer_lock::WriterLockError),
}

/// Outcome of a full dream cycle (filesystem + post phases).
#[derive(Debug)]
pub struct DreamCycleResult {
    pub decay: DecayResult,
    pub autobiography: Option<AutobiographyOutcome>,
}

/// How post-filesystem index updates reach the Tantivy indexer task.
pub enum IndexBackend {
    /// Reuse the daemon's live indexer channel (HTTP path).
    Sender(tokio::sync::mpsc::Sender<crate::server::tantivy_handle::IndexerMsg>),
    /// Open a fresh handle for this operation under the caller's project
    /// writer lock (CLI in-process path).
    FreshHandle(crate::writer_lock::WriterLock),
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

/// Reject a cycle while `state.json` says one is in progress (HTTP 409 guard).
///
/// In the daemon this is an early check: the coordinator's `PrepareCycle`
/// runs it again inside the actor, together with the actor's own open cycle,
/// and that is the authoritative one.
pub fn ensure_not_in_progress(agent_root: &AgentRoot) -> Result<(), DreamCycleError> {
    match wal::read_last_cycle_status(agent_root)? {
        status if status == "in_progress" => Err(DreamCycleError::InProgress),
        _ => Ok(()),
    }
}

/// Filesystem phases: consolidation then decay, as one plan.
///
/// [`prepare`], then [`compose`], then [`commit`], in this task. The model call
/// runs before the stage opens, so the stage and `in_progress` cover only the
/// commit. The commit plans every output and stages it in one stage with one
/// redo WAL, so the full cycle is a single atomic region (ARCH-2). The CLI's
/// in-process path calls this under the project writer lock; the daemon runs
/// the same three steps through its coordinator actor instead, which must
/// reopen its append fd afterward.
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
///
/// `ledger` is the project writer lock's appender (`WriterLock::ledger`); the
/// plan's provenance edges go through it at commit.
pub async fn run_filesystem_phases(
    agent_root: &AgentRoot,
    ledger: &Ledger,
    now_sec: i64,
    cycle_date: &str,
    no_llm: bool,
    share_personal: bool,
) -> Result<DecayResult, DreamCycleError> {
    let prepared = prepare(agent_root, now_sec)?;
    let body = compose(
        agent_root,
        prepared.selected.as_ref(),
        no_llm,
        share_personal,
    )
    .await;
    let composed_for = prepared.selected.as_ref().map(|s| s.cluster_key.as_str());
    commit(
        agent_root,
        ledger,
        now_sec,
        cycle_date,
        &prepared.raw,
        composed_for,
        body,
    )
}

/// [`run_filesystem_phases`] with an explicit backend — the seam tests use to
/// drive the LLM path with a fake and no network.
///
/// `share_personal` is `false` for every pre-AILAB-199 caller; the flag exists
/// here so a test can drive the consent path against the same fake backend the
/// rest of the LLM assertions use.
pub async fn run_filesystem_phases_with_backend<B: LlmBackend>(
    agent_root: &AgentRoot,
    ledger: &Ledger,
    now_sec: i64,
    cycle_date: &str,
    backend: &B,
    model: &str,
    share_personal: bool,
) -> Result<DecayResult, DreamCycleError> {
    let prepared = prepare(agent_root, now_sec)?;
    let body = match prepared.selected.as_ref() {
        Some(selected) => {
            // The spend cap (AILAB-196) is read here rather than threaded
            // through this function, because its signature is the FakeLlm seam
            // and its callers hold a backend, not a project config. Reading it
            // inside this arm also keeps a cycle that promotes nothing free of
            // the extra file read. Same degrade-to-defaults rule as
            // [`compose`]: a broken config.toml must never fail a cycle.
            let cap_usd = crate::config::load_config(agent_root.project_root())
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "config load failed; using the default LLM cost cap");
                    crate::config::Config::default()
                })
                .cost_cap_usd;
            compose_lesson_body(
                selected,
                Some((backend, model)),
                cap_usd,
                agent_root,
                share_personal,
            )
            .await
        }
        None => LessonBodySource::Deterministic,
    };
    let composed_for = prepared.selected.as_ref().map(|s| s.cluster_key.as_str());
    commit(
        agent_root,
        ledger,
        now_sec,
        cycle_date,
        &prepared.raw,
        composed_for,
        body,
    )
}

/// What a cycle composes for (BZR-1241): the log's lines, read once, and the
/// lesson the model is asked to write.
#[derive(Debug)]
pub struct Prepared {
    pub raw: Vec<episodic::RawLine>,
    /// `None`: nothing promotes.
    pub selected: Option<SelectedLesson>,
}

/// The lesson a cycle over `raw` composes for. Pure. The coordinator's
/// `PrepareCycle` calls it on the lines it read itself.
#[must_use]
pub fn select(raw: &[episodic::RawLine], now_sec: i64) -> Option<SelectedLesson> {
    consolidation::select_lesson(
        &consolidation::compute_promoted_clusters(&plan::records(raw), now_sec),
        now_sec,
    )
}

/// Read the log once (an absent file is empty) and select. Reads no other file.
pub fn prepare(agent_root: &AgentRoot, now_sec: i64) -> Result<Prepared, DreamCycleError> {
    let bytes = match std::fs::read(agent_root.episodic_jsonl()) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(WalError::Io(e).into()),
    };
    let raw = episodic::raw_lines(&bytes);
    let selected = select(&raw, now_sec);
    Ok(Prepared { raw, selected })
}

/// The lesson body for `selected`, composed outside any actor. `None`, or
/// `no_llm`, is the deterministic body. Otherwise `config.toml` is loaded once
/// (degrading to defaults, as today), the backend is resolved, and
/// `compose_lesson_body` runs with that config's `cost_cap_usd`. Infallible.
pub async fn compose(
    agent_root: &AgentRoot,
    selected: Option<&SelectedLesson>,
    no_llm: bool,
    share_personal: bool,
) -> LessonBodySource {
    // `None` = nothing promoted: the plan retires any stale LESSONS.md
    // (AILAB-699) and decay still runs, with no body to compose.
    let Some(selected) = selected else {
        return LessonBodySource::Deterministic;
    };
    if no_llm {
        // `share_personal` is deliberately dropped on this arm rather than
        // forwarded: with no backend there is no payload to consent to, and
        // reading `personal/` here would be a disclosure with no recipient.
        return LessonBodySource::Deterministic;
    }

    // A broken config.toml must not fail a cycle that would otherwise run
    // deterministically — degrade to defaults and say so.
    let config = crate::config::load_config(agent_root.project_root()).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "config load failed; using defaults for the LLM phase");
        crate::config::Config::default()
    });

    match llm::resolve_backend(&config) {
        Some(backend) => {
            compose_lesson_body(
                selected,
                Some((&backend, config.model.as_str())),
                config.cost_cap_usd,
                agent_root,
                share_personal,
            )
            .await
        }
        None => LessonBodySource::Deterministic,
    }
}

/// Stage and commit one cycle over `raw` (the whole log as of now). Opens the
/// stage (`begin_cycle`: status `in_progress`), stages the autosnap and the
/// unlinks of the `snap-*` refs past the retention cap
/// ([`retention::plan_ref_prune`]), reads the prior `LESSONS.md` through the
/// stage, plans with `body` gated against the plan's selection, applies the
/// plan, and commits it. Then it removes the objects no ref names
/// ([`retention::collect_objects`]). Reads no log.
///
/// `composed_for` is the cluster key `body` was composed for (see
/// [`gate_at_commit`]).
pub fn commit(
    agent_root: &AgentRoot,
    ledger: &Ledger,
    now_sec: i64,
    cycle_date: &str,
    raw: &[episodic::RawLine],
    composed_for: Option<&str>,
    body: LessonBodySource,
) -> Result<DecayResult, DreamCycleError> {
    // ARCH-2: ONE plan holds BOTH filesystem phases' outputs, in ONE stage.
    // begin_cycle guards `.agent/` existence (WEG-281), refuses a pending WAL
    // and sets state=in_progress; nothing live changes until commit_cycle
    // writes the redo WAL, applies it and sets state=complete. A read, plan or
    // stage error short-circuits via `?` BEFORE that commit point, leaving only
    // the stage for the next writer to wipe (state=failed) — so no
    // half-finished cycle is ever recorded "complete", and none is ever
    // half-applied.
    let mut stage = wal::begin_cycle(agent_root, CycleKind::Dream, now_sec)?;
    // BZR-160 / BZR-1241: the pre-cycle image, taken at the commit, so it
    // holds every record that landed while the model ran.
    snapshot::stage_autosnap(&mut stage, now_sec)?;
    // BZR-1245: the `snap-*` refs past the newest AUTOSNAP_KEEP are unlinked
    // through the stage, so they go with this cycle's commit or not at all.
    let staged = snapshot::autosnap_ref_name(now_sec)?;
    match retention::plan_ref_prune(agent_root, &staged) {
        Ok(names) => {
            for name in names {
                stage
                    .unlink(&agent_root.branches_refs_dir().join(name))
                    .map_err(WalError::Io)?;
            }
        }
        Err(e) => retention::warn_skipped(&e, "autosnap retention skipped; no ref pruned"),
    }
    let prior = stage.read(&agent_root.lessons_md()).map_err(WalError::Io)?;
    let plan = plan::plan_cycle_with(raw, prior.as_deref(), now_sec, |selected| {
        gate_at_commit(body, composed_for, selected)
    })
    .map_err(LessonPhaseError::from)?;
    stage.apply_plan(&plan, cycle_date).map_err(WalError::Io)?;
    // BZR-1240: the redo WAL carries the plan's ledger edges, and the commit
    // appends them after the renames; a roll-forward appends them again, and
    // the ledger skips an edge it already holds.
    wal::commit_cycle(stage, ledger, now_sec)?;
    // BZR-1245: objects are not part of the commit. Collection is idempotent,
    // so whatever a crash or an error leaves, the next cycle collects. The
    // wall clock is read only for the grace cutoff.
    let cutoff = SystemTime::now()
        .checked_sub(retention::OBJECT_GC_GRACE)
        .unwrap_or(UNIX_EPOCH);
    if let Err(e) = retention::collect_objects(agent_root, cutoff) {
        retention::warn_skipped(&e, "autosnap object collection skipped");
    }
    Ok(plan.decay_result())
}

/// BZR-1241: the citation gate, re-run against the commit plan's selection.
/// Records that landed during the compose can move the top cluster or its
/// members (deepest-wins), so a model body survives only when the plan selects
/// the cluster it was composed for and every citation is still a member.
pub(crate) fn gate_at_commit(
    body: LessonBodySource,
    composed_for: Option<&str>,
    selected: &SelectedLesson,
) -> LessonBodySource {
    let LessonBodySource::Llm { citations, .. } = &body else {
        return body;
    };
    let members: Vec<String> = selected
        .events
        .iter()
        .map(|e| e.id.as_str().to_string())
        .collect();
    if composed_for == Some(selected.cluster_key.as_str())
        && llm::citations_are_valid(citations, &members)
    {
        return body;
    }
    tracing::warn!(
        composed_for = ?composed_for,
        cluster_key = %selected.cluster_key,
        cited = ?citations,
        "the commit plan no longer matches the composed lesson; writing the deterministic body"
    );
    LessonBodySource::Deterministic
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

/// Inputs to [`run_guarded_cycle`] that do not vary by entry point.
pub struct GuardedCycleOptions<'a> {
    pub agent_root: &'a AgentRoot,
    pub project_root: &'a Path,
    /// Caller-supplied clock; `cycle_date` is derived from it here.
    pub now_sec: i64,
    /// `false` on `--no-commit`: skips the dirty-tree walk and the autobiography
    /// commit.
    pub commit_autobiography: bool,
    pub index: IndexBackend,
}

/// The one dream-cycle sequencer (BZR-172). Every real cycle — `POST
/// /api/v1/dream` and the CLI in-process path — runs through here:
///
///   1. 409 guard ([`ensure_not_in_progress`]);
///   2. `cycle_date` from [`cycle_date_from_now_sec`];
///   3. dirty-path capture, only when the autobiography will commit (WEG-63);
///   4. `dispatch(now_sec, cycle_date)` — the filesystem phases;
///   5. [`run_post_phases`] with the decay result.
///
/// `dispatch` is the adapter. The daemon's callback is `dispatch_cycle` (the
/// `POST /api/v1/dream` handler): it sends `PrepareCycle` to the coordinator
/// actor, composes the body in its own task, then sends `CommitCycle`, and the
/// actor commits and reopens its append fd (WEG-271). A full inbox at Prepare
/// maps to [`DreamCycleError::CoordinatorBusy`], and a cycle the actor already
/// holds open to [`DreamCycleError::InProgress`]. The CLI's callback calls
/// [`run_filesystem_phases`] directly. This function never opens a stage
/// itself — the one stage and its commit stay inside the filesystem phases.
///
/// Queue property: appends wait only while the actor handles `PrepareCycle` or
/// `CommitCycle`, which are file I/O with no model call; the model call runs
/// between the two. Index and autobiography run here, after the actor has
/// replied, so they never hold the coordinator's inbox.
pub async fn run_guarded_cycle<F, Fut>(
    opts: GuardedCycleOptions<'_>,
    dispatch: F,
) -> Result<DreamCycleResult, DreamCycleError>
where
    F: FnOnce(i64, String) -> Fut,
    Fut: std::future::Future<Output = Result<DecayResult, DreamCycleError>>,
{
    ensure_not_in_progress(opts.agent_root)?;
    let cycle_date = cycle_date_from_now_sec(opts.now_sec);

    // WEG-63 — capture dirty state BEFORE the cycle runs.
    let dirty_at_cycle_start = if opts.commit_autobiography {
        autobiography::check_dirty_at_cycle_start(opts.project_root).unwrap_or_default()
    } else {
        Vec::new()
    };

    let decay = dispatch(opts.now_sec, cycle_date.clone()).await?;

    let autobiography = run_post_phases(PostPhaseOptions {
        agent_root: opts.agent_root,
        project_root: opts.project_root,
        cycle_date: &cycle_date,
        decay_result: &decay,
        dirty_at_cycle_start: &dirty_at_cycle_start,
        commit_autobiography: opts.commit_autobiography,
        index: opts.index,
    })
    .await?;

    Ok(DreamCycleResult {
        decay,
        autobiography,
    })
}

/// Full in-process cycle for the CLI (`--no-commit` or no daemon).
///
/// Takes the project writer lock before anything else, so it refuses while
/// `dreamd watch`, an in-process `dreamd mcp`, or another CLI writer holds the
/// project, and writes nothing when it does. Taking the lock also resets a
/// stale `in_progress` status, rolls a committed cycle's WAL forward and wipes
/// any staged one.
///
/// ONE current-thread runtime drives the whole cycle. Since AILAB-204 the
/// filesystem phases are async too: [`run_filesystem_phases`] runs prepare,
/// compose and commit in this task, and the compose `await`s the model call
/// before the stage opens. They share the runtime that already existed for the
/// index phases — building a second one inside the first would panic at the
/// nested `block_on`.
pub fn run_in_process(
    project_root: &Path,
    now_sec: i64,
    no_commit: bool,
    no_llm: bool,
    share_personal: bool,
) -> Result<DreamCycleResult, DreamCycleError> {
    let agent_root = AgentRoot::new(project_root);
    let lock = crate::writer_lock::WriterLock::acquire(
        &agent_root,
        crate::writer_lock::WriterKind::Dream,
    )?;
    let root = &agent_root;
    let ledger = lock.ledger();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime for dream cycle");

    let outcome = runtime.block_on(run_guarded_cycle(
        GuardedCycleOptions {
            agent_root: root,
            project_root,
            now_sec,
            commit_autobiography: !no_commit,
            index: IndexBackend::FreshHandle(lock.clone()),
        },
        |now_sec, cycle_date| async move {
            run_filesystem_phases(root, ledger, now_sec, &cycle_date, no_llm, share_personal).await
        },
    ));
    // The indexer task holds a clone of the token; dropping the runtime ends
    // that task, so the project stays locked until every task is gone.
    drop(runtime);
    drop(lock);
    outcome
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
/// Deliberately absent: the stage and its WAL, the recurrence sidecar, the retire
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

async fn run_index_phases(
    agent_root: &AgentRoot,
    decay_result: &DecayResult,
    index: IndexBackend,
) -> Result<(), DreamCycleError> {
    match index {
        IndexBackend::Skip => Ok(()),
        IndexBackend::Sender(sender) => {
            if !decay_result.decayed_ids.is_empty() {
                prune_decayed_events(&sender, decay_result.decayed_ids.clone()).await?;
            }
            // DR-211: consolidation rewrote LESSONS.md earlier in this cycle;
            // fold the new lesson set into the live index without a restart.
            index_semantic_lessons(agent_root, &sender).await?;
            Ok(())
        }
        IndexBackend::FreshHandle(lock) => {
            use crate::server::{TantivyIndexHandle, DEFAULT_COMMIT_CADENCE};

            let handle = TantivyIndexHandle::open(agent_root, &lock, DEFAULT_COMMIT_CADENCE)?;
            if !decay_result.decayed_ids.is_empty() {
                handle
                    .prune_decayed_events(decay_result.decayed_ids.clone())
                    .await?;
            }
            // `open()` above already ran the semantic pass; re-running it after
            // the prune commit is idempotent (delete every semantic document,
            // re-add the current lesson set).
            handle.index_semantic_lessons(agent_root.clone()).await?;
            Ok(())
        }
    }
}

/// Ask the live indexer to re-index `semantic/LESSONS.md` (DR-211 / AILAB-205).
/// Sender-only mirror of [`TantivyIndexHandle::index_semantic_lessons`] for the
/// daemon path, which holds a channel rather than the handle.
///
/// Unconditional: there is no "file present" pre-check, because the pass must
/// also run to clear stale semantic documents after a cycle that removed
/// LESSONS.md.
///
/// [`TantivyIndexHandle::index_semantic_lessons`]: crate::server::TantivyIndexHandle::index_semantic_lessons
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

    /// The project writer lock on `root`, for the ledger the filesystem
    /// phases append through. A fixture root has no WAL and no `state.json`,
    /// so taking it recovers nothing.
    fn writer_lock(root: &AgentRoot) -> crate::writer_lock::WriterLock {
        crate::writer_lock::WriterLock::acquire(root, crate::writer_lock::WriterKind::Dream)
            .expect("project writer lock")
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
            writer_lock(root).ledger(),
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

        let result = run_in_process(&project_root, NOW_SEC, true, true, false)
            .expect("full in-process cycle");

        assert!(root.lessons_md().exists());
        assert!(!result.decay.decayed_ids.is_empty() || result.decay.kept_count > 0);
    }

    /// BZR-160: a real cycle leaves one object and one `snap-*` ref, taken at
    /// the commit before any output is staged, and never the branch pointer
    /// file.
    #[test]
    fn in_process_cycle_writes_one_autosnap() {
        let (_dir, root) = scaffold_fixture();
        let project_root = root.project_root().to_path_buf();
        let jsonl_before = fs::read(root.episodic_jsonl()).unwrap();

        run_in_process(&project_root, NOW_SEC, true, true, false).expect("full cycle");

        let refs: Vec<_> = fs::read_dir(root.branches_refs_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(refs, ["snap-20250513t120000z"]);
        let body = fs::read_to_string(root.branches_refs_dir().join(&refs[0])).unwrap();
        let id = body.lines().next().unwrap();
        let obj = root.branches_objects_dir().join(id);
        assert!(obj.join(snapshot::MANIFEST_FILE).is_file());
        // Pre-mutation image: the fixture had no LESSONS.md before the cycle.
        assert!(!obj.join("semantic/LESSONS.md").exists());
        assert_eq!(
            fs::read(obj.join("episodic/AGENT_LEARNINGS.jsonl")).unwrap(),
            jsonl_before
        );
        assert!(!root.branches_dir().join("HEAD").exists());
    }

    /// BZR-1245: with 20 older `snap-*` refs, a cycle unlinks the oldest one
    /// and collects its object, which is past the grace and named by no ref.
    #[tokio::test]
    async fn cycle_prunes_snap_refs_and_collects_their_objects() {
        let (_dir, root) = scaffold_fixture();
        fs::create_dir_all(root.semantic_dir()).unwrap();
        let mut seeded = Vec::new();
        for i in 0..20 {
            // A distinct LESSONS.md per autosnap, so each names its own object.
            fs::write(root.lessons_md(), format!("seed {i}\n")).unwrap();
            let t = NOW_SEC - 86_400 + i * 60;
            let id = snapshot::create_autosnap(&root, t).unwrap();
            seeded.push((snapshot::autosnap_ref_name(t).unwrap(), id));
        }
        fs::remove_file(root.lessons_md()).unwrap();
        let two_hours_ago = SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        for (_, id) in &seeded {
            fs::File::open(root.branches_objects_dir().join(id))
                .unwrap()
                .set_modified(two_hours_ago)
                .unwrap();
        }

        run_deterministic(&root).await;

        let refs: Vec<String> = fs::read_dir(root.branches_refs_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(refs.len(), 20, "{refs:?}");
        assert!(refs.iter().all(|r| r.starts_with("snap-")), "{refs:?}");
        assert!(!refs.contains(&seeded[0].0), "the oldest ref is pruned");
        let staged = snapshot::autosnap_ref_name(NOW_SEC).unwrap();
        assert!(refs.contains(&staged), "the cycle's own ref lands");

        let objects = root.branches_objects_dir();
        assert!(
            !objects.join(&seeded[0].1).exists(),
            "the pruned ref's object is collected"
        );
        for (_, id) in &seeded[1..] {
            assert!(objects.join(id).is_dir(), "{id} is still named");
        }
        let body = fs::read_to_string(root.branches_refs_dir().join(&staged)).unwrap();
        let new_id = body.lines().next().unwrap();
        assert!(seeded.iter().all(|(_, id)| id != new_id));
        assert!(objects.join(new_id).is_dir());
        assert_eq!(fs::read_dir(&objects).unwrap().count(), 20);
    }

    /// Test 13's store: 20 `snap-*` refs a day old, each naming its own object,
    /// every object two hours old. Returns `(ref name, object id)` oldest first.
    fn seed_old_autosnaps(root: &AgentRoot) -> Vec<(String, String)> {
        fs::create_dir_all(root.semantic_dir()).unwrap();
        let mut seeded = Vec::new();
        for i in 0..20 {
            fs::write(root.lessons_md(), format!("seed {i}\n")).unwrap();
            let t = NOW_SEC - 86_400 + i * 60;
            let id = snapshot::create_autosnap(root, t).unwrap();
            seeded.push((snapshot::autosnap_ref_name(t).unwrap(), id));
        }
        fs::remove_file(root.lessons_md()).unwrap();
        let two_hours_ago = SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        for (_, id) in &seeded {
            fs::File::open(root.branches_objects_dir().join(id))
                .unwrap()
                .set_modified(two_hours_ago)
                .unwrap();
        }
        seeded
    }

    /// An object dir no ref names, holding one manifest, with mtime `mtime`.
    fn unnamed_object(root: &AgentRoot, fill: u64, mtime: SystemTime) -> PathBuf {
        let dir = root.branches_objects_dir().join(format!("{fill:064x}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(snapshot::MANIFEST_FILE), b"{}\n").unwrap();
        fs::File::open(&dir).unwrap().set_modified(mtime).unwrap();
        dir
    }

    fn snap_refs(root: &AgentRoot) -> Vec<String> {
        let mut refs: Vec<String> = fs::read_dir(root.branches_refs_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with("snap-"))
            .collect();
        refs.sort();
        refs
    }

    /// BZR-1245 Amendment 3, test 22: a retention plan that fails only warns.
    /// The cycle commits and prunes nothing.
    #[tokio::test]
    async fn cycle_commits_when_retention_planning_fails() {
        let (_dir, root) = scaffold_fixture();
        let seeded = seed_old_autosnaps(&root);
        fs::write(crate::bisect::state_path(&root), "{not json").unwrap();

        run_deterministic(&root).await;

        assert!(root.lessons_md().exists(), "the cycle committed");
        let refs = snap_refs(&root);
        assert_eq!(refs.len(), 21, "nothing pruned: {refs:?}");
        assert!(refs.contains(&snapshot::autosnap_ref_name(NOW_SEC).unwrap()));
        for (name, id) in &seeded {
            assert!(refs.contains(name), "{name} is kept");
            assert!(
                root.branches_objects_dir().join(id).is_dir(),
                "{id} is kept"
            );
        }
    }

    /// BZR-1245 Amendment 3, test 23: object collection that fails after the
    /// commit only warns. The prune still lands, and nothing is removed.
    #[tokio::test]
    async fn cycle_commits_when_object_collection_fails() {
        let (_dir, root) = scaffold_fixture();
        let seeded = seed_old_autosnaps(&root);
        fs::write(root.branches_refs_dir().join("broken"), "junk\n").unwrap();
        let two_hours_ago = SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        let x = unnamed_object(&root, 0xa, two_hours_ago);

        run_deterministic(&root).await;

        assert!(root.lessons_md().exists(), "the cycle committed");
        let refs = snap_refs(&root);
        assert_eq!(refs.len(), 20, "{refs:?}");
        assert!(!refs.contains(&seeded[0].0), "the oldest ref is pruned");
        assert!(
            root.branches_objects_dir().join(&seeded[0].1).is_dir(),
            "collection failed closed: the pruned ref's object stays"
        );
        assert!(x.is_dir(), "collection failed closed: X stays");
    }

    /// BZR-1245 Amendment 3, test 24: the cycle collects with the grace, so a
    /// fresh unnamed object survives and an old one goes.
    #[tokio::test]
    async fn cycle_keeps_a_fresh_unnamed_object() {
        let (_dir, root) = scaffold_fixture();
        seed_old_autosnaps(&root);
        let f = unnamed_object(&root, 0xf, SystemTime::now());
        let two_hours_ago = SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        let o = unnamed_object(&root, 0x0, two_hours_ago);

        run_deterministic(&root).await;

        assert!(f.is_dir(), "F is inside the grace");
        assert!(!o.exists(), "O is unnamed and past the grace");
    }

    #[test]
    fn preview_leaves_branches_absent() {
        let (_dir, root) = scaffold_fixture();
        let project_root = root.project_root().to_path_buf();

        preview_in_process(&project_root, NOW_SEC, true, false).expect("preview");

        assert!(!root.branches_dir().exists(), "--dry must not snapshot");
    }

    #[test]
    fn cycle_date_from_now_sec_is_utc_yyyy_mm_dd() {
        assert_eq!(cycle_date_from_now_sec(0), "1970-01-01");
        // 2025-05-13T12:00:00Z — the fixture clock used throughout this module.
        assert_eq!(cycle_date_from_now_sec(NOW_SEC), "2025-05-13");
    }

    /// BZR-172 seam: the sequencer guards, dispatches exactly once, and runs
    /// the post phases — with no daemon, no index, and no autobiography.
    #[tokio::test]
    async fn guarded_cycle_dispatches_once_through_the_callback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = AgentRoot::new(dir.path());
        fs::create_dir_all(root.agent_dir()).unwrap();

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let result = run_guarded_cycle(
            GuardedCycleOptions {
                agent_root: &root,
                project_root: dir.path(),
                now_sec: NOW_SEC,
                commit_autobiography: false,
                index: IndexBackend::Skip,
            },
            |now_sec, cycle_date| {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(now_sec, NOW_SEC);
                assert_eq!(cycle_date, "2025-05-13");
                async { Ok(DecayResult::default()) }
            },
        )
        .await;

        let result = result.expect("guarded cycle");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(result.decay.decayed_ids.is_empty());
        assert!(result.autobiography.is_none());
    }

    #[test]
    fn guarded_cycle_rejects_an_active_cycle_before_dispatch() {
        let (_dir, root) = scaffold_fixture();
        wal::begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = runtime
            .block_on(run_guarded_cycle(
                GuardedCycleOptions {
                    agent_root: &root,
                    project_root: root.project_root(),
                    now_sec: NOW_SEC,
                    commit_autobiography: false,
                    index: IndexBackend::Skip,
                },
                |_, _| async { panic!("dispatch must not run while a cycle is in progress") },
            ))
            .unwrap_err();
        assert!(matches!(err, DreamCycleError::InProgress));
    }

    #[tokio::test]
    async fn single_wal_envelope_committed_and_cleaned_after_full_cycle() {
        let (_dir, root) = scaffold_fixture();
        run_deterministic(&root).await;
        assert!(
            !root.wal_path().exists(),
            "the one stage committed + WAL cleaned"
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

        run_in_process(&project_root, NOW_SEC, true, true, false).expect("real cycle");

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
        consolidation::preview_select_lesson(root, NOW_SEC)
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
            writer_lock(&root).ledger(),
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
            writer_lock(&root_b).ledger(),
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
            writer_lock(&root_b).ledger(),
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
            writer_lock(&root_b).ledger(),
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
            writer_lock(&root_b).ledger(),
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
            writer_lock(&root_b).ledger(),
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
            writer_lock(&root_b).ledger(),
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
            writer_lock(&root).ledger(),
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
            writer_lock(&root).ledger(),
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

        // `--no-llm` stops one frame up, in `compose`, which never hands this
        // function a backend; `llm_target = None` is that path. A backend exists
        // here only so "it was not called" is asserted against an object that
        // records calls.
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
            writer_lock(&root).ledger(),
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
            writer_lock(&root).ledger(),
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

    /// The log, `state.json`, `LESSONS.md`, and every file under
    /// `.dreamd/index/`; absent files are recorded as absent.
    fn store_bytes(
        root: &AgentRoot,
    ) -> std::collections::BTreeMap<std::path::PathBuf, Option<Vec<u8>>> {
        let mut paths = vec![root.episodic_jsonl(), root.state_json(), root.lessons_md()];
        if let Ok(entries) = fs::read_dir(root.dreamd_dir().join(crate::server::INDEX_DIR_NAME)) {
            paths.extend(entries.map(|e| e.unwrap().path()));
        }
        paths
            .into_iter()
            .map(|p| {
                let bytes = fs::read(&p).ok();
                (p, bytes)
            })
            .collect()
    }

    /// `dream --no-commit` runs in this process. While `dreamd watch` holds
    /// the project it must refuse before it writes anything, instead of
    /// rewriting the log under the daemon and failing only at the index.
    #[cfg(unix)]
    #[test]
    fn in_process_cycle_refuses_while_watch_holds_the_project() {
        use crate::writer_lock::{WriterKind, WriterLock};

        let (_dir, root) = scaffold_fixture();
        let project_root = root.project_root().to_path_buf();
        run_in_process(&project_root, NOW_SEC, true, true, false).expect("first cycle");
        let before = store_bytes(&root);

        let _watch = WriterLock::acquire(&root, WriterKind::Watch).unwrap();
        let err = run_in_process(&project_root, NOW_SEC + 60, true, true, false).unwrap_err();

        assert!(
            err.to_string()
                .contains(&format!("pid {} (watch)", std::process::id())),
            "the refusal must name the holder; got: {err}"
        );
        assert_eq!(store_bytes(&root), before, "a refused cycle writes nothing");
    }

    /// BZR-1240 Amendment 1 item 2: the cycle reads `LESSONS.md` before it
    /// stages anything, so a path it cannot read fails the cycle before the
    /// commit point. Before BZR-1240 the rename onto the directory failed
    /// after it, and every recovery retried it.
    #[cfg(unix)]
    #[test]
    fn a_directory_at_lessons_md_fails_the_cycle_before_the_commit_point() {
        let (_dir, root) = scaffold_fixture();
        assert!(
            consolidation::preview_select_lesson(&root, NOW_SEC)
                .unwrap()
                .is_some(),
            "precondition: the fixture promotes a cluster"
        );
        fs::create_dir_all(root.lessons_md()).unwrap();
        let log_before = fs::read(root.episodic_jsonl()).unwrap();
        let lock = writer_lock(&root);

        let result = consolidation::run_deterministic_dream_cycle(&root, lock.ledger(), NOW_SEC);

        assert!(result.is_err(), "an unreadable LESSONS.md fails the cycle");
        assert!(
            !root.wal_path().exists(),
            "the cycle never reached its commit point"
        );
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), log_before);
    }

    // ── BZR-1241: the citation gate at commit ────────────────────────────────

    fn gate_member(tail: &str) -> dreamd_protocol::AgentLearning {
        dreamd_protocol::AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: dreamd_protocol::EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5{tail}"))
                .unwrap(),
            timestamp: chrono::DateTime::from_timestamp(NOW_SEC, 0).unwrap(),
            pain: 6.0,
            importance: 7.0,
            pinned: false,
            skill_action: "rust::eh".to_string(),
            source_harness: "test".to_string(),
            content: format!("member {tail}"),
        }
    }

    /// The commit plan's selection: `rust::eh` with members FA0, FA1, FA2.
    fn commit_selection() -> SelectedLesson {
        let events = vec![gate_member("FA0"), gate_member("FA1"), gate_member("FA2")];
        SelectedLesson {
            cluster_key: "rust::eh".to_string(),
            exemplar_id: events[2].id.as_str().to_string(),
            exemplar_content: events[2].content.clone(),
            events,
        }
    }

    fn composed(citations: &[&str]) -> LessonBodySource {
        LessonBodySource::Llm {
            content: "Return `?` instead of unwrapping.".to_string(),
            prompt_version: llm::VERSIONED_PROMPT_ID.to_string(),
            citations: citations
                .iter()
                .map(|tail| format!("evt_01ARZ3NDEKTSV4RRFFQ69G5{tail}"))
                .collect(),
        }
    }

    #[test]
    fn gate_keeps_a_body_whose_cluster_and_citations_survive() {
        let body = composed(&["FA0", "FA2"]);
        assert_eq!(
            gate_at_commit(body.clone(), Some("rust::eh"), &commit_selection()),
            body
        );
        assert_eq!(
            gate_at_commit(LessonBodySource::Deterministic, None, &commit_selection()),
            LessonBodySource::Deterministic
        );
    }

    #[test]
    fn gate_drops_a_body_when_the_top_cluster_moves() {
        // Every citation is still a member; only the cluster differs.
        assert_eq!(
            gate_at_commit(
                composed(&["FA0"]),
                Some("rust::eh::io"),
                &commit_selection()
            ),
            LessonBodySource::Deterministic
        );
    }

    #[test]
    fn gate_drops_a_body_whose_citation_left_the_cluster() {
        assert_eq!(
            gate_at_commit(
                composed(&["FA0", "FA9"]),
                Some("rust::eh"),
                &commit_selection()
            ),
            LessonBodySource::Deterministic
        );
    }

    #[test]
    fn ensure_not_in_progress_rejects_active_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let root = AgentRoot::new(dir.path());
        fs::create_dir_all(root.dreamd_dir()).unwrap();
        wal::begin_cycle(&root, CycleKind::Dream, NOW_SEC).unwrap();

        let err = ensure_not_in_progress(&root).unwrap_err();
        assert!(matches!(err, DreamCycleError::InProgress));
    }
}
