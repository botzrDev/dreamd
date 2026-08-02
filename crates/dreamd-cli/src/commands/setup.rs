//! `dreamd setup` — the front door: scaffold `.agent/`, then print the harness
//! wiring next steps (AILAB-549).
//!
//! The scaffold is [`super::init::run`] verbatim, so `dreamd init` stdout stays
//! byte-locked against `tests/fixtures/init.golden.txt` (Clip A): setup wraps
//! init's lines with its own header/footer and never rewrites them.
//!
//! The harness MCP config is merged by [`super::setup_mcp`] under the ratified
//! AILAB-548 taxonomy. On a TTY without `--yes`, [`run`] first walks the
//! AILAB-551 wizard prompts; off a TTY it refuses rather than silently
//! defaulting, using the same vocabulary as `dreamd reset` (§1.2).
//!
//! A real run closes with the frozen AILAB-548 §1 success card. Everything it
//! claims is observed, not assumed: `wired` lists the labels
//! [`wire_mcp`] actually wrote, and `daemon` reports a UDS liveness probe on
//! the socket the daemon itself would bind. A `--dry-run` prints the plan and
//! the next-steps list instead — a plan is not a completed setup.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use dreamd_core::{AgentRoot, DaemonHome};

use super::init::{self, find_project_root, InitError};
use super::setup_mcp::{self, Action, McpError, TargetPlan};

/// Floating-pin MCP server block printed as copy-paste guidance. `npx -y`
/// re-resolves the `latest` dist-tag on each spawn — never hard-pin here
/// (AGENTS.md `npm-dreamd-mcp-unscoped`).
const MCP_BLOCK: &str = r#"{"mcpServers":{"dreamd":{"command":"npx","args":["-y","dreamd-mcp"]}}}"#;

/// Left margin of every success-card row.
const CARD_GUTTER: &str = "  ";
/// Column every card value starts at, gutter included — the one number the
/// card's shape is defined by. A test recomputes it from the rendered rows, so
/// a longer label can never quietly knock one value out of the column.
const CARD_VALUE_COLUMN: usize = 15;
/// Width of the label field that lands values on [`CARD_VALUE_COLUMN`]. Wide
/// enough that `scaffolded` (the longest label) still leaves a visible gap.
const CARD_LABEL_WIDTH: usize = CARD_VALUE_COLUMN - CARD_GUTTER.len();

/// How many times we re-probe the socket after spawning the daemon, and how
/// long we wait between probes — ~2s worst case before the card gives up and
/// reports `not running`.
const WATCH_PROBE_ATTEMPTS: u32 = 20;
const WATCH_PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// Which harness(es) to wire up — i.e. which MCP config files get the
/// `mcpServers.dreamd` block.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Cursor,
    Both,
    None,
}

impl Harness {
    /// Project-relative MCP config files this harness reads (adapter READMEs).
    /// Empty for [`Harness::None`]. The global `~/.cursor/mcp.json` is out of
    /// scope by design (AILAB-548 §2) — `setup` only writes inside the project.
    pub(crate) fn config_targets(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &[".mcp.json"],
            Self::Cursor => &[".cursor/mcp.json"],
            Self::Both => &[".mcp.json", ".cursor/mcp.json"],
            Self::None => &[],
        }
    }

    /// Product name to say out loud in the success card. The flag values are
    /// lowercase identifiers; the card talks to a person.
    fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Both => "Claude Code and Cursor",
            // Unreachable from the wired card (no targets means nothing wired),
            // but the mapping stays total rather than panicking.
            Self::None => "your harness",
        }
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::Both => "both",
            Self::None => "none",
        };
        f.write_str(s)
    }
}

/// Failure modes for `dreamd setup`. Mirrors [`InitError`] so the exit-code
/// contract (`2` usage / `1` runtime) is unchanged from `dreamd init`.
#[derive(Debug)]
pub enum SetupError {
    NoProjectRoot,
    /// No TTY to prompt on and no `--yes`. Refusing beats silently accepting
    /// defaults that write files (AILAB-551 §1.2); `cli.rs` maps this to `2`.
    NotATty,
    Io(std::io::Error),
    /// The harness MCP config could not be written (conflict without `--force`,
    /// malformed existing JSON, or an unwritable path). Details are already on
    /// stderr; `cli.rs` maps this to exit code 1.
    Mcp(McpError),
}

impl From<std::io::Error> for SetupError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<InitError> for SetupError {
    fn from(e: InitError) -> Self {
        match e {
            InitError::NoProjectRoot => Self::NoProjectRoot,
            InitError::Io(e) => Self::Io(e),
        }
    }
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProjectRoot => write!(f, "no project root"),
            // Byte-identical to `ResetError::NotATty` on purpose — one refusal
            // vocabulary across commands (AILAB-551 §1.2). A test pins the pair.
            Self::NotATty => write!(f, "stdin is not a tty; pass --yes to confirm"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Mcp(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SetupError {}

/// Which prompts to run. A flag the user typed on the command line wins over
/// its prompt (AILAB-551 §1.7) — detected via `ValueSource::CommandLine`,
/// never by comparing against clap defaults.
#[derive(Debug, Clone, Copy, Default)]
pub struct Interactive {
    pub ask_harness: bool,
    pub ask_watch: bool,
}

/// Everything [`run`] needs, injected by the caller. `cli.rs` resolves the real
/// home-derived daemon path; tests pass temp dirs.
///
/// Every field is `Copy`, so [`run`] can rebuild the request with the
/// post-prompt harness/watch values via struct-update syntax instead of
/// mutating through the shared reference.
pub struct SetupRequest<'a> {
    /// Directory the command was invoked from; the project root is resolved by
    /// walking up from here (same sentinels as `dreamd init`).
    pub cwd: &'a Path,
    /// Daemon home (normally `~/.agent`) holding `registry.toml`.
    pub daemon_home: &'a Path,
    pub harness: Harness,
    /// `false` when `--no-write-mcp` was given.
    pub write_mcp: bool,
    /// `--force`: replace an incompatible `mcpServers.dreamd` entry. Never
    /// overrides a JSON parse failure (AILAB-548 §6C).
    pub force: bool,
    /// `--start-watch` was requested: spawn `dreamd watch` in the background
    /// after wiring, then report what the liveness probe actually finds. Never
    /// honoured under `dry_run`.
    pub start_watch: bool,
    /// Print the plan without writing anything.
    pub dry_run: bool,
    /// `--yes`: accept the flag values as given and never touch stdin.
    pub yes: bool,
    /// Which prompts remain after the explicitly-typed flags claim their own.
    /// Ignored when `yes` is set.
    pub interactive: Interactive,
}

/// `dreamd setup` entry point. Order: resolve project root → prompt (TTY, no
/// `--yes`) → echo the resolved plan → scaffold (or print the dry-run plan) →
/// next steps.
pub fn run(
    req: &SetupRequest<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), SetupError> {
    let project_root = match find_project_root(req.cwd) {
        Some(r) => r,
        None => {
            writeln!(
                err,
                "dreamd: error \u{2014} no project root found. Run from inside a project directory (must contain .git/, Cargo.toml, package.json, or pyproject.toml). If this is a new folder, run `git init` first."
            )?;
            return Err(SetupError::NoProjectRoot);
        }
    };

    writeln!(
        out,
        "dreamd setup \u{2014} project root: {}",
        project_root.display()
    )?;

    // The answers the run actually uses. `--yes` keeps the flag values verbatim
    // and never reaches a `read_line`; the prompts only ever narrow these two.
    let mut harness = req.harness;
    let mut start_watch = req.start_watch;

    if !req.yes {
        if !std::io::stdin().is_terminal() {
            // Same line as `dreamd reset` — refusing beats writing files off a
            // pipe with answers nobody gave (AILAB-551 §1.2).
            writeln!(err, "error: stdin is not a tty; pass --yes to confirm")?;
            return Err(SetupError::NotATty);
        }
        // Ratified: `--dry-run` prints the plan for the flags as given. Asking
        // questions to then write nothing is noise.
        if !req.dry_run {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            // The prompts go to stderr (as `dreamd reset` does), so the header
            // line above has to be pushed out of stdout's buffer first —
            // otherwise under `dreamd setup > log.txt` the question appears on
            // screen with no context, and the context lands in the file after
            // the run. Flushing here orders the two streams as the user reads
            // them.
            out.flush()?;
            if req.interactive.ask_harness {
                harness = prompt_harness(&mut input, err)?;
            }
            if req.interactive.ask_watch {
                start_watch = prompt_start_watch(&mut input, err)?;
            }
        }
    }

    // Rebind with the post-prompt answers so every downstream step (MCP
    // targets, dry-run plan, next steps) reads one settled request.
    let settled = SetupRequest {
        harness,
        start_watch,
        ..*req
    };
    let req = &settled;

    writeln!(out, "harness: {}", req.harness)?;

    if req.dry_run {
        let agent_dir = AgentRoot::new(&project_root).agent_dir();
        if agent_dir.exists() {
            writeln!(
                out,
                "dry-run: {} already exists \u{2014} scaffold would be skipped",
                agent_dir.display()
            )?;
        } else {
            writeln!(out, "dry-run: would scaffold {}", agent_dir.display())?;
            writeln!(
                out,
                "dry-run: would register the project in {}",
                DaemonHome::new(req.daemon_home).registry_toml().display()
            )?;
        }
        wire_mcp(req, &project_root, out, err)?;
        writeln!(out, "dry-run: no changes made")?;
        // A dry run is a plan, not a completed setup — the success card would
        // be a lie here, so the plan keeps the next-steps list.
        write_next_steps(req, out)?;
    } else {
        // Reuse init verbatim (non-quiet): the scaffold lines and the DR-413
        // privacy disclosure are exactly what a first-run user needs to see.
        init::run(req.cwd, req.daemon_home, false, out, err)?;
        let wired = wire_mcp(req, &project_root, out, err)?;
        let daemon_live = start_watch_and_probe(req, &project_root, err)?;
        report_doctor(req.cwd, out)?;
        write_success_card(&wired, req.harness, daemon_live, out)?;
    }

    Ok(())
}

/// The wizard's harness default. Deliberately NOT the clap default
/// (`Harness::Both`): a user at a prompt is answering for the agent in front of
/// them, while a scripted `--yes` run wants both configs wired (AILAB-551 §1.6).
const HARNESS_PROMPT_DEFAULT: Harness = Harness::Claude;

/// Stand-in for a line we could not read at all. No `parse_*` accepts it, so an
/// unreadable answer walks the ordinary re-ask-then-default path instead of
/// ending the run. The leading NUL keeps it out of anything a user could type.
const UNREADABLE_ANSWER: &str = "\u{0}<unreadable>";

/// Read one line, trimmed. Infallible by design — the module's contract is
/// re-ask once, then default, never fail:
///
/// * `Ok(0)` is EOF (closed stdin) and yields `""`, which every caller reads as
///   "take the default" — never as "ask again".
/// * `Err` yields [`UNREADABLE_ANSWER`], which no parser accepts. `read_line`
///   returns `ErrorKind::InvalidData` for non-UTF-8 bytes, so a stray `0xFF`
///   pasted at the prompt must not abort a front-door install with exit 1; a
///   genuine stdin I/O failure degrades the same way, to the default.
fn read_answer(input: &mut dyn BufRead) -> String {
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(_) => line.trim().to_string(),
        Err(_) => UNREADABLE_ANSWER.to_string(),
    }
}

/// Prompts are written to stderr, never stdout — the TTY gate only checks
/// *stdin*, so under `dreamd setup > log.txt` on a terminal a stdout prompt
/// would render nothing while `read_line` blocked, and the question would end
/// up in the file. `dreamd reset` prompts on stderr for the same reason
/// (AILAB-551 §1.2 — one refusal/prompt vocabulary across commands).
fn write_harness_prompt(err: &mut dyn Write) -> std::io::Result<()> {
    writeln!(err)?;
    writeln!(err, "  Which coding agent are you wiring up?")?;
    writeln!(err, "    1) Claude Code        \u{2192} .mcp.json")?;
    writeln!(err, "    2) Cursor             \u{2192} .cursor/mcp.json")?;
    writeln!(err, "    3) Both")?;
    writeln!(err, "    4) Skip \u{2014} I'll wire it myself")?;
    // No newline: the cursor sits after the colon, where the user types.
    write!(err, "  [1-4, default 1]: ")?;
    err.flush()
}

fn parse_harness_answer(answer: &str) -> Option<Harness> {
    match answer {
        "" => Some(HARNESS_PROMPT_DEFAULT),
        "1" => Some(Harness::Claude),
        "2" => Some(Harness::Cursor),
        "3" => Some(Harness::Both),
        "4" => Some(Harness::None),
        _ => None,
    }
}

/// Ask which harness to wire. Re-asks exactly once on unparseable input, then
/// takes the default rather than looping (AILAB-548 §1) — a wizard that cannot
/// be escaped is worse than one that guesses.
pub(crate) fn prompt_harness(
    input: &mut dyn BufRead,
    err: &mut dyn Write,
) -> std::io::Result<Harness> {
    write_harness_prompt(err)?;
    if let Some(harness) = parse_harness_answer(&read_answer(input)) {
        return Ok(harness);
    }
    write_harness_prompt(err)?;
    Ok(parse_harness_answer(&read_answer(input)).unwrap_or(HARNESS_PROMPT_DEFAULT))
}

/// Written to stderr for the same reason as [`write_harness_prompt`].
fn write_watch_prompt(err: &mut dyn Write) -> std::io::Result<()> {
    writeln!(err)?;
    writeln!(
        err,
        "  Start the shared memory daemon now? It serializes writes across agents."
    )?;
    write!(err, "  [y/N]: ")?;
    err.flush()
}

fn parse_yes_no(answer: &str) -> Option<bool> {
    match answer.to_ascii_lowercase().as_str() {
        // Enter takes the capitalised default in `[y/N]`.
        "" | "n" | "no" => Some(false),
        "y" | "yes" => Some(true),
        _ => None,
    }
}

/// Ask whether to start the daemon. Same one-re-ask-then-default rule as
/// [`prompt_harness`]; the default is "no" to match `[y/N]`.
pub(crate) fn prompt_start_watch(
    input: &mut dyn BufRead,
    err: &mut dyn Write,
) -> std::io::Result<bool> {
    write_watch_prompt(err)?;
    if let Some(answer) = parse_yes_no(&read_answer(input)) {
        return Ok(answer);
    }
    write_watch_prompt(err)?;
    Ok(parse_yes_no(&read_answer(input)).unwrap_or(false))
}

/// Merge `mcpServers.dreamd` into every harness config target (or report the
/// dry-run plan). Runs after the scaffold so a refusal here never leaves a
/// half-made `.agent/`.
///
/// `--harness both` is all-or-nothing (AILAB-548 §6A): every target is read and
/// classified before any of them is written.
///
/// Returns the labels that end up carrying the dreamd block — what the success
/// card reports as `wired`, and what its `Undo:` line names. Empty when wiring
/// was skipped (`--no-write-mcp` / `--harness none`) or under `--dry-run`, where
/// nothing was written and there is nothing to undo.
fn wire_mcp(
    req: &SetupRequest<'_>,
    project_root: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Vec<&'static str>, SetupError> {
    let targets = req.harness.config_targets();
    if !req.write_mcp {
        writeln!(out, "mcp config: skipped (--no-write-mcp)")?;
        return Ok(Vec::new());
    }
    if targets.is_empty() {
        writeln!(out, "mcp config: skipped (--harness none)")?;
        return Ok(Vec::new());
    }

    let mut plans = Vec::with_capacity(targets.len());
    for label in targets {
        match setup_mcp::plan(project_root, label) {
            Ok(planned) => plans.push(planned),
            // Unparseable config: refuse before writing anything, `--force`
            // included (AILAB-548 §6C).
            Err(e) => return Err(report_mcp_error(e, err)?),
        }
    }

    if req.dry_run {
        for planned in &plans {
            report_dry_run(req, planned, out)?;
        }
        return Ok(Vec::new());
    }

    if !req.force {
        let blocker = plans.iter().find_map(|planned| match &planned.action {
            Action::Conflict(reason) => Some(McpError::Conflict {
                label: planned.label,
                reason: reason.clone(),
            }),
            _ => None,
        });
        if let Some(e) = blocker {
            return Err(report_mcp_error(e, err)?);
        }
    }

    // Everything still standing here gets written: Create, Merge, AlreadyWired
    // (a no-op that leaves the block in place), and — past the guard above —
    // a `--force`-resolved Conflict. All four leave the file wired.
    let mut wired = Vec::with_capacity(plans.len());
    for planned in &plans {
        if let Err(e) = setup_mcp::apply(planned) {
            return Err(report_mcp_error(e, err)?);
        }
        writeln!(out, "mcp config: {}", applied_status(planned))?;
        wired.push(planned.label);
    }
    Ok(wired)
}

/// Explain the refusal on stderr, then hand back the error for the exit code.
fn report_mcp_error(e: McpError, err: &mut dyn Write) -> std::io::Result<SetupError> {
    writeln!(err, "dreamd: error \u{2014} {e}")?;
    match &e {
        McpError::Malformed { .. } => writeln!(
            err,
            "dreamd: nothing was written. Fix or remove the file, then re-run \u{2014} --force cannot override a JSON parse error."
        )?,
        McpError::Conflict { .. } => writeln!(
            err,
            "dreamd: nothing was written. Re-run with --force to replace the \"dreamd\" entry (other MCP servers are preserved)."
        )?,
        McpError::Io { .. } => {}
    }
    Ok(SetupError::Mcp(e))
}

/// Status line for a target we just wrote (or deliberately did not).
fn applied_status(planned: &TargetPlan) -> String {
    match &planned.action {
        Action::Create => format!("wrote {}", planned.label),
        Action::Merge => format!("merged into {}", planned.label),
        Action::AlreadyWired => format!("{} already wired \u{2014} no change", planned.label),
        Action::Conflict(reason) => format!(
            "replaced the conflicting \"dreamd\" entry in {} (--force; was: {reason})",
            planned.label
        ),
    }
}

/// Per-target dry-run report: the verdict plus the JSON we would write.
fn report_dry_run(
    req: &SetupRequest<'_>,
    planned: &TargetPlan,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    match &planned.action {
        Action::Create => writeln!(out, "dry-run: would create {}", planned.label)?,
        Action::Merge => writeln!(out, "dry-run: would merge into {}", planned.label)?,
        Action::AlreadyWired => writeln!(
            out,
            "dry-run: {} already wired \u{2014} no change",
            planned.label
        )?,
        Action::Conflict(reason) => {
            writeln!(
                out,
                "dry-run: CONFLICT: {} \u{2014} {reason}",
                planned.label
            )?;
            if req.force {
                writeln!(out, "dry-run: --force would replace the \"dreamd\" entry")?;
            } else {
                // Nothing would be written here, so do not print a document.
                return writeln!(
                    out,
                    "dry-run: re-run with --force to replace the \"dreamd\" entry"
                );
            }
        }
    }
    if let Some(contents) = &planned.contents {
        write!(out, "{contents}")?;
    }
    Ok(())
}

/// Launch `dreamd watch` in the background. Deliberately NOT a daemon: the CLI
/// crate forbids `unsafe`, so there is no `setsid` available and the child stays
/// in setup's session. It outlives setup, but a terminal close can SIGHUP it —
/// documented, and the success card reports what is actually live rather than
/// what we hoped for. `dreamd watch` in the foreground remains the supported
/// long-running form.
fn spawn_watch(project_root: &Path) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("watch")
        .current_dir(project_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// The daemon socket, resolved exactly the way `status` and `doctor` resolve it
/// (`$DREAMD_SOCK` else the daemon-home socket). Never derived from
/// `req.daemon_home`: under `$DREAMD_SOCK` that would name a path the daemon
/// itself is not listening on, and the card would lie.
#[cfg(unix)]
fn resolve_socket() -> Option<PathBuf> {
    dreamd_core::client::resolve_daemon_socket()
}

/// No UDS on Windows in v0.1 — there is no socket to resolve, so the card
/// always reports the daemon down. Mirrors the `cli::run_doctor` arm.
#[cfg(not(unix))]
fn resolve_socket() -> Option<PathBuf> {
    None
}

/// Liveness probe for the resolved socket — a bounded connect, so an orphan
/// socket file left by a `SIGKILL` reports dead instead of hanging.
#[cfg(unix)]
fn socket_is_live(path: &Path) -> bool {
    dreamd_core::server::is_daemon_socket_live(path)
}

/// Windows fallback so the command compiles; mirrors `status::daemon_liveness`.
#[cfg(not(unix))]
fn socket_is_live(path: &Path) -> bool {
    path.exists()
}

/// Is a daemon listening right now? One resolve, one probe, no waiting.
fn daemon_is_live() -> bool {
    resolve_socket().is_some_and(|path| socket_is_live(&path))
}

/// Start the daemon when asked, then report whether one is actually listening.
///
/// The card reports the probe, not the request: a spawn that failed, a daemon
/// that has not bound yet, and a daemon someone else started all read the same
/// way to the user, because all three are answered by the same question — is
/// anything serving the socket?
fn start_watch_and_probe(
    req: &SetupRequest<'_>,
    project_root: &Path,
    err: &mut dyn Write,
) -> std::io::Result<bool> {
    if !req.start_watch {
        // No spawn: a daemon may already be up from an earlier session, so ask
        // once rather than assuming it is down.
        return Ok(daemon_is_live());
    }

    if daemon_is_live() {
        // Something is already serving the socket. A second `dreamd watch`
        // would lose the `bind_writer_socket` race with `AlreadyBound` and exit
        // 1 into fully nulled stdio — a guaranteed-doomed process nobody would
        // ever see. Report the daemon that is actually up instead.
        return Ok(true);
    }

    if let Err(e) = spawn_watch(project_root) {
        // Advisory only. The scaffold and the MCP wiring already succeeded, and
        // the card will honestly say `not running`.
        writeln!(
            err,
            "dreamd: could not start the daemon in the background ({e}) \u{2014} run `dreamd watch` yourself."
        )?;
        return Ok(daemon_is_live());
    }

    // The child has to bind before it is live; poll instead of guessing a
    // single sleep, and return on the first success.
    for _ in 0..WATCH_PROBE_ATTEMPTS {
        std::thread::sleep(WATCH_PROBE_INTERVAL);
        if daemon_is_live() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Advisory `doctor` pass: one verdict line, never an exit code, never a
/// mutation. Ratified as advisory-only, so every way this can fail — no config,
/// no discoverable store, doctor itself erroring — skips the beat instead of
/// failing a setup whose real work is already done.
///
/// Doctor's own report is long enough to bury the success card, so it is
/// captured and summarised rather than printed.
fn report_doctor(cwd: &Path, out: &mut dyn Write) -> std::io::Result<()> {
    let Ok(config) = dreamd_core::config::load_config(cwd) else {
        return Ok(());
    };
    let Ok(agent_root) = AgentRoot::discover(cwd) else {
        return Ok(());
    };
    let skip = dreamd_core::autobiography::read_last_skip(&agent_root);
    let socket = resolve_socket();
    let now_sec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut report = Vec::new();
    let mut report_err = Vec::new();
    let all_ok = super::doctor::run(
        &config,
        &agent_root,
        skip.as_ref(),
        socket.as_deref(),
        // `repair: false` is load-bearing: a diagnostic beat the user did not
        // ask for must never unlink a socket or rebuild an index.
        super::doctor::DoctorFlags {
            repair: false,
            cluster_health: false,
        },
        now_sec,
        &mut report,
        &mut report_err,
    );
    match all_ok {
        Ok(true) => writeln!(out, "doctor: all checks passed"),
        Ok(false) => writeln!(
            out,
            "doctor: warnings found \u{2014} run `dreamd doctor` for detail"
        ),
        // A diagnostic that cannot run is not a setup failure.
        Err(_) => Ok(()),
    }
}

/// One `label   value` row of the success card. The label sits in a fixed-width
/// field so every value lands on [`CARD_VALUE_COLUMN`].
fn write_card_row(out: &mut dyn Write, label: &str, value: &str) -> std::io::Result<()> {
    writeln!(out, "{CARD_GUTTER}{label:<CARD_LABEL_WIDTH$}{value}")
}

/// The AILAB-548 §1 success card — frozen copy, printed once, at the end of a
/// real (non-dry-run) setup.
///
/// Pure by construction: the wired labels, the harness, and daemon liveness are
/// all passed in, so the copy is unit-testable without touching a filesystem or
/// a socket. `wired` empty means nothing was written for the user (`--harness
/// none`, `--no-write-mcp`, or wizard choice 4), which swaps the `wired` rows
/// and the `Undo:` block for the copy-paste MCP block.
fn write_success_card(
    wired: &[&'static str],
    harness: Harness,
    daemon_live: bool,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    // Blank line first: the card must not run into init's scaffold output.
    writeln!(out)?;
    writeln!(out, "\u{2713} dreamd is set up.")?;
    writeln!(out)?;

    write_card_row(out, "scaffolded", ".agent/")?;
    for label in wired {
        write_card_row(
            out,
            "wired",
            &format!("{label}  (dreamd \u{2192} npx -y dreamd-mcp)"),
        )?;
    }
    write_card_row(
        out,
        "daemon",
        if daemon_live {
            "running"
        } else {
            "not running \u{2014} start it with: dreamd watch"
        },
    )?;
    writeln!(out)?;

    if wired.is_empty() {
        // Same rule as `write_next_steps`: name the files the chosen harness
        // reads, or fall back to a generic phrase when there are none.
        let targets = harness.config_targets();
        let where_to = if targets.is_empty() {
            "your harness MCP config".to_string()
        } else {
            targets.join(" and ")
        };
        writeln!(out, "  Next: add the dreamd MCP server to {where_to}:")?;
        writeln!(out, "          {MCP_BLOCK}")?;
        writeln!(out, "        then reload your harness and ask your agent:")?;
    } else {
        writeln!(
            out,
            "  Next: reload {}, then ask your agent:",
            harness.display_name()
        )?;
    }
    // Byte-identical to adapters/cursor/README.md §6 — one verify sentence
    // across the docs and the CLI.
    writeln!(
        out,
        "        \"What has dreamd remembered about this codebase?\""
    )?;
    writeln!(out, "        You should see it call search_nodes.")?;

    if !wired.is_empty() {
        writeln!(out)?;
        writeln!(
            out,
            "  Undo: remove the \"dreamd\" block from {}.",
            wired.join(" and ")
        )?;
        writeln!(
            out,
            "        Re-run with --no-write-mcp to skip config writes entirely."
        )?;
    }
    Ok(())
}

fn write_next_steps(req: &SetupRequest<'_>, out: &mut dyn Write) -> std::io::Result<()> {
    let targets = req.harness.config_targets();
    // Only hand out the copy-paste block when we did NOT write the config
    // ourselves (`--no-write-mcp` / `--harness none`).
    let show_mcp_step = !req.write_mcp || targets.is_empty();

    writeln!(out)?;
    writeln!(out, "next steps:")?;
    writeln!(
        out,
        "  1. start the shared daemon (one serialized writer): dreamd watch"
    )?;
    if req.start_watch {
        // Only reachable under `--dry-run`, which starts nothing by definition;
        // the real path spawns the daemon and the card reports the probe.
        writeln!(
            out,
            "     note: --start-watch would start it \u{2014} dry-run starts nothing"
        )?;
    }
    let verify_step = if show_mcp_step {
        let where_to = if targets.is_empty() {
            "your harness MCP config".to_string()
        } else {
            targets.join(" and ")
        };
        writeln!(out, "  2. add the dreamd MCP server to {where_to}:")?;
        writeln!(out, "       {MCP_BLOCK}")?;
        3
    } else {
        2
    };
    writeln!(
        out,
        "  {verify_step}. reload your harness, then ask your agent to call search_nodes"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Drive a prompt with canned bytes on stdin, returning the answer + the
    /// prompt copy. The sink is the *err* sink: prompts are written to stderr
    /// (see [`write_harness_prompt`]), so a test capturing stdout would capture
    /// nothing.
    fn ask_bytes<T>(
        prompt: fn(&mut dyn BufRead, &mut dyn Write) -> std::io::Result<T>,
        typed: &[u8],
    ) -> (T, String) {
        let mut input = Cursor::new(typed.to_vec());
        let mut err = Vec::new();
        let answer = prompt(&mut input, &mut err).expect("prompt must not fail on an in-memory io");
        (
            answer,
            String::from_utf8(err).expect("prompt copy is utf-8"),
        )
    }

    /// [`ask_bytes`] for the ordinary case: what a person typed.
    fn ask<T>(
        prompt: fn(&mut dyn BufRead, &mut dyn Write) -> std::io::Result<T>,
        typed: &str,
    ) -> (T, String) {
        ask_bytes(prompt, typed.as_bytes())
    }

    /// Marker unique to one rendering of the harness prompt, so counting it
    /// counts re-asks.
    const HARNESS_MARKER: &str = "  [1-4, default 1]: ";
    const WATCH_MARKER: &str = "  [y/N]: ";

    #[test]
    fn setup_not_a_tty_matches_reset_vocabulary() {
        assert_eq!(
            SetupError::NotATty.to_string(),
            "stdin is not a tty; pass --yes to confirm"
        );
        // AILAB-551 §1.2: the two commands must refuse in the same words, so
        // drift in either one fails here rather than shipping.
        assert_eq!(
            SetupError::NotATty.to_string(),
            super::super::reset::ResetError::NotATty.to_string(),
            "setup and reset must share one non-tty refusal line"
        );
    }

    #[test]
    fn harness_prompt_bare_enter_is_claude() {
        let (harness, printed) = ask(prompt_harness, "\n");
        assert_eq!(
            harness,
            Harness::Claude,
            "the wizard default is claude, not the clap default both"
        );
        assert_eq!(
            printed.matches(HARNESS_MARKER).count(),
            1,
            "a valid answer must not re-ask; got: {printed:?}"
        );
    }

    #[test]
    fn harness_prompt_maps_each_choice() {
        for (typed, expected) in [
            ("1\n", Harness::Claude),
            ("2\n", Harness::Cursor),
            ("3\n", Harness::Both),
            ("4\n", Harness::None),
        ] {
            let (harness, _) = ask(prompt_harness, typed);
            assert_eq!(harness, expected, "choice {typed:?} must map to {expected}");
        }
    }

    #[test]
    fn harness_prompt_re_asks_once_then_accepts() {
        let (harness, printed) = ask(prompt_harness, "banana\n3\n");
        assert_eq!(harness, Harness::Both, "the second, valid answer wins");
        assert_eq!(
            printed.matches(HARNESS_MARKER).count(),
            2,
            "exactly one re-ask; got: {printed:?}"
        );
    }

    #[test]
    fn harness_prompt_two_bad_answers_fall_back_without_looping() {
        let (harness, printed) = ask(prompt_harness, "banana\nkiwi\n");
        assert_eq!(
            harness,
            Harness::Claude,
            "a second unparseable answer takes the default, never a third ask"
        );
        assert_eq!(
            printed.matches(HARNESS_MARKER).count(),
            2,
            "must not loop past one re-ask; got: {printed:?}"
        );
    }

    #[test]
    fn harness_prompt_eof_is_the_default() {
        // Closed stdin: `read_line` returns Ok(0) forever. Treating that as a
        // blank answer is what keeps this from spinning.
        let (harness, printed) = ask(prompt_harness, "");
        assert_eq!(harness, Harness::Claude);
        assert_eq!(
            printed.matches(HARNESS_MARKER).count(),
            1,
            "EOF is a blank answer, not an error to re-ask; got: {printed:?}"
        );
    }

    #[test]
    fn harness_prompt_survives_invalid_utf8() {
        // `BufRead::read_line` returns `ErrorKind::InvalidData` on non-UTF-8
        // bytes. Propagating that would surface as `SetupError::Io` and end the
        // install with exit 1 and no `.agent/` — a stray `0xFF` at the prompt
        // must cost a re-ask, not the run.
        let mut input = Cursor::new(vec![0xFF, b'\n', b'3', b'\n']);
        let mut err = Vec::new();
        let answer = prompt_harness(&mut input, &mut err);
        assert!(
            answer.is_ok(),
            "an unreadable line must never fail the prompt; got: {answer:?}"
        );
        assert_eq!(
            answer.unwrap(),
            Harness::Both,
            "the bad bytes are consumed through the newline, so the re-ask reads \
             the following good line"
        );
        let printed = String::from_utf8(err).expect("prompt copy is utf-8");
        assert_eq!(
            printed.matches(HARNESS_MARKER).count(),
            2,
            "exactly one re-ask, same as any other unparseable answer; got: {printed:?}"
        );
    }

    #[test]
    fn harness_prompt_invalid_utf8_alone_takes_the_default() {
        // Nothing readable follows, so the re-ask hits EOF and the run lands on
        // the wizard default rather than erroring out.
        let (harness, _) = ask_bytes(prompt_harness, &[0xFF, b'\n']);
        assert_eq!(harness, HARNESS_PROMPT_DEFAULT);
    }

    #[test]
    fn harness_prompt_copy_is_the_ratified_block() {
        let (_, printed) = ask(prompt_harness, "1\n");
        assert_eq!(
            printed,
            "\n  Which coding agent are you wiring up?\n    1) Claude Code        \u{2192} .mcp.json\n    2) Cursor             \u{2192} .cursor/mcp.json\n    3) Both\n    4) Skip \u{2014} I'll wire it myself\n  [1-4, default 1]: ",
            "AILAB-548 §1 copy is frozen"
        );
    }

    #[test]
    fn watch_prompt_answers() {
        for (typed, expected) in [
            ("\n", false),
            ("y\n", true),
            ("YES\n", true),
            ("n\n", false),
            ("banana\nY\n", true),
            ("banana\nbanana\n", false),
            ("", false),
        ] {
            let (answer, _) = ask(prompt_start_watch, typed);
            assert_eq!(answer, expected, "{typed:?} must answer {expected}");
        }
    }

    #[test]
    fn watch_prompt_re_asks_at_most_once() {
        let (_, printed) = ask(prompt_start_watch, "banana\nbanana\n");
        assert_eq!(
            printed.matches(WATCH_MARKER).count(),
            2,
            "exactly one re-ask, then the default; got: {printed:?}"
        );
    }

    #[test]
    fn watch_prompt_copy_is_the_ratified_block() {
        let (_, printed) = ask(prompt_start_watch, "y\n");
        assert_eq!(
            printed,
            "\n  Start the shared memory daemon now? It serializes writes across agents.\n  [y/N]: ",
            "AILAB-548 §1 copy is frozen"
        );
    }

    /// Render the card into a String. Pure — no filesystem, no daemon.
    fn card(wired: &[&'static str], harness: Harness, daemon_live: bool) -> String {
        let mut out = Vec::new();
        write_success_card(wired, harness, daemon_live, &mut out)
            .expect("card writing must not fail on an in-memory io");
        String::from_utf8(out).expect("card copy is utf-8")
    }

    /// The card is frozen AILAB-548 §1 copy, so it is asserted whole — the same
    /// way the two prompts are. Substring checks let blank-line layout and the
    /// `Next:`/`Undo:` indentation drift without a single test going red; this
    /// is the assertion that makes any card edit deliberate.
    #[test]
    fn wired_card_copy_is_frozen() {
        assert_eq!(
            card(&[".mcp.json"], Harness::Claude, false),
            "\n\u{2713} dreamd is set up.\n\
             \n  \
             scaffolded   .agent/\n  \
             wired        .mcp.json  (dreamd \u{2192} npx -y dreamd-mcp)\n  \
             daemon       not running \u{2014} start it with: dreamd watch\n\
             \n  \
             Next: reload Claude Code, then ask your agent:\n        \
             \"What has dreamd remembered about this codebase?\"\n        \
             You should see it call search_nodes.\n\
             \n  \
             Undo: remove the \"dreamd\" block from .mcp.json.\n        \
             Re-run with --no-write-mcp to skip config writes entirely.\n",
            "AILAB-548 §1 card copy is frozen"
        );
    }

    /// The `--no-write-mcp` shape: a harness was chosen but nothing was written,
    /// so the `wired` rows and the `Undo:` block give way to the copy-paste MCP
    /// block naming the file that harness reads.
    #[test]
    fn skipped_card_copy_is_frozen() {
        assert_eq!(
            card(&[], Harness::Claude, false),
            "\n\u{2713} dreamd is set up.\n\
             \n  \
             scaffolded   .agent/\n  \
             daemon       not running \u{2014} start it with: dreamd watch\n\
             \n  \
             Next: add the dreamd MCP server to .mcp.json:\n          \
             {\"mcpServers\":{\"dreamd\":{\"command\":\"npx\",\"args\":[\"-y\",\"dreamd-mcp\"]}}}\n        \
             then reload your harness and ask your agent:\n        \
             \"What has dreamd remembered about this codebase?\"\n        \
             You should see it call search_nodes.\n",
            "AILAB-548 §1 card copy is frozen"
        );
    }

    #[test]
    fn card_lists_every_wired_target_and_undoes_both() {
        let rendered = card(&[".mcp.json", ".cursor/mcp.json"], Harness::Both, false);
        assert_eq!(
            rendered
                .lines()
                .filter(|l| l.starts_with("  wired"))
                .count(),
            2,
            "one row per wired target; got: {rendered}"
        );
        assert!(
            rendered.contains(
                "  Undo: remove the \"dreamd\" block from .mcp.json and .cursor/mcp.json."
            ),
            "Undo must name every file we touched; got: {rendered}"
        );
    }

    #[test]
    fn card_reports_a_live_daemon_without_a_start_hint() {
        let rendered = card(&[".mcp.json"], Harness::Claude, true);
        assert!(
            rendered.contains("  daemon       running"),
            "got: {rendered}"
        );
        assert!(
            !rendered.contains("start it with"),
            "a running daemon needs no start hint; got: {rendered}"
        );
    }

    #[test]
    fn card_spells_out_the_harness_by_name() {
        for (harness, expected) in [
            (Harness::Claude, "Claude Code"),
            (Harness::Cursor, "Cursor"),
            (Harness::Both, "Claude Code and Cursor"),
        ] {
            let rendered = card(harness.config_targets(), harness, false);
            assert!(
                rendered.contains(&format!("  Next: reload {expected}, then ask your agent:")),
                "{harness} must read as {expected:?}; got: {rendered}"
            );
        }
    }

    /// Column where a card row's value starts: past the two-space gutter, the
    /// label word, and the padding that follows it. Computed, never eyeballed.
    fn value_column(line: &str) -> usize {
        let after_gutter = &line[CARD_GUTTER.len()..];
        let label_len = after_gutter
            .find(' ')
            .expect("a card row is `label` then padding then value");
        let padding =
            after_gutter[label_len..].len() - after_gutter[label_len..].trim_start().len();
        CARD_GUTTER.len() + label_len + padding
    }

    #[test]
    fn card_rows_share_one_value_column() {
        let rendered = card(&[".mcp.json"], Harness::Claude, false);
        let columns: Vec<usize> = ["  scaffolded", "  wired", "  daemon"]
            .iter()
            .map(|prefix| {
                let line = rendered
                    .lines()
                    .find(|l| l.starts_with(prefix))
                    .unwrap_or_else(|| panic!("card must have a {prefix:?} row; got: {rendered}"));
                value_column(line)
            })
            .collect();
        assert_eq!(
            columns,
            vec![CARD_VALUE_COLUMN; 3],
            "scaffolded/wired/daemon values must align; got: {rendered}"
        );
    }

    #[test]
    fn card_copy_never_claims_dreamd_learns() {
        // The launch-copy rule, pinned at the unit level too so a card edit
        // fails here before it reaches the integration suite.
        for wired in [&[][..], &[".mcp.json"][..]] {
            let lowered = card(wired, Harness::Claude, false).to_lowercase();
            for banned in ["learns", "learned", "reflects", "thinks"] {
                assert!(
                    !lowered.contains(banned),
                    "card must never say {banned:?}; got: {lowered}"
                );
            }
        }
    }
}
