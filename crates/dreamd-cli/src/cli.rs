//! Top-level CLI dispatch for the `dreamd` binary.
//!
//! Parses args via clap, routes to subcommand handlers, and maps errors to
//! exit codes. Exit code contract:
//!   - `0` -- success
//!   - `1` -- runtime / I/O error
//!   - `2` -- usage error (missing subcommand, no project root)

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Args, Parser, Subcommand};

use dreamd_core::collector::RecallExclusion;
use dreamd_core::config::{load_config, Config, DreamCycleMode};

use crate::commands;
use crate::commands::version::VERSION_SHORT;

/// Root CLI parser. Routes to subcommands or handles `--version`.
#[derive(Parser)]
#[command(
    name = "dreamd",
    disable_version_flag = true,
    about = "Portable memory layer for AI coding agents"
)]
pub struct Cli {
    /// Print version information.
    #[arg(short = 'V', long = "version", action = ArgAction::SetTrue, global = false)]
    pub version: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Arguments for the `dreamd dream` subcommand.
#[derive(Args, Debug)]
pub struct DreamArgs {
    /// Dry run: print the LESSONS.md this cycle would write, without writing
    /// anything. Skips the daemon proxy; --no-commit is ignored.
    #[arg(long)]
    pub dry: bool,
    /// Not supported. Cycles run only when invoked.
    #[arg(long, hide = true)]
    pub auto: bool,
    /// Skip the git commit and the daemon. The cycle runs in this process.
    /// Do not use it while dreamd watch is serving this project.
    #[arg(long)]
    pub no_commit: bool,
    /// Force deterministic mode even when an API key is present.
    #[arg(long)]
    pub no_llm: bool,
    /// Include .agent/personal/ in the dream-cycle composition prompt for this
    /// call. Off by default: the personal layer never reaches a model unless
    /// this flag is passed. Capped at 16 KiB and ignored under --no-llm.
    #[arg(long)]
    pub share_personal: bool,
}

impl DreamArgs {
    /// Validate flag combinations. Returns `Err` with a user-facing message if
    /// the combination is invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.dry && self.auto {
            return Err("--dry and --auto are mutually exclusive: \
                 auto mode performs real writes on a schedule"
                .to_string());
        }
        Ok(())
    }
}

/// Arguments for the `dreamd forget` subcommand (BZR-151).
#[derive(Args, Debug)]
pub struct ForgetArgs {
    /// The event id to remove (`evt_` + 26 Crockford base32 characters).
    #[arg(value_name = "EVENT_ID")]
    pub id: String,
    /// Print what would be removed without writing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Write a forget-receipt/1.0 JSON file (event id, lesson effect, ledger
    /// root, orphan edges) to PATH after a removal. Not a signed proof.
    #[arg(long, value_name = "PATH", conflicts_with = "dry_run")]
    pub proof: Option<PathBuf>,
}

/// Arguments for the `dreamd watch` subcommand.
///
/// Both flags configure the TCP listener, which only the off-Unix `watch` has
/// (AILAB-197). On Unix `watch` serves `~/.agent/dreamd.sock` and either flag is
/// a usage error (exit 2). `bind` stays a `String` here and is parsed in
/// `commands::watch`, so a hostname is refused there rather than resolved.
#[derive(Args, Debug, Default)]
pub struct WatchArgs {
    /// TCP listen address (not Unix): an IP or IP:port, e.g. 127.0.0.1:8080 or
    /// [::1]:0. Port defaults to 0 (OS-chosen). Hostnames are not resolved.
    /// Default: 127.0.0.1:0.
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<String>,
    /// Allow a non-loopback --bind (not Unix). Logs a warning on every start.
    /// Does not disable the bearer token, and ~/.agent/server.json still names
    /// a loopback address.
    #[arg(long)]
    pub insecure: bool,
}

/// Arguments for the `dreamd doctor` subcommand.
#[derive(Args, Debug, Default)]
pub struct DoctorArgs {
    /// Rebuild Tantivy from JSONL; unlink orphaned UDS. Wipes the whole derived
    /// index cache dir, including any .tantivy-*.lock inside it, and never targets
    /// lock files on their own. Does not modify AGENT_LEARNINGS.jsonl.
    #[arg(long)]
    pub repair: bool,
    /// Print skill_action prefix counts vs semantic/recurrence_counts.json; flag drift.
    #[arg(long)]
    pub cluster_health: bool,
    /// Recompute the provenance Merkle root and list orphan edges, missing
    /// lesson and recurrence edges, and corrupt ledger lines. Reads the ledger.
    /// Does not rewrite it.
    #[arg(long)]
    pub provenance: bool,
}

/// Top-level subcommands exposed by the `dreamd` binary.
#[derive(Subcommand)]
pub enum Command {
    /// Maintain the on-disk memory log. Today: unpin entries with --force-unpin.
    Archive(ArchiveArgs),
    /// Show which stored memories drove a query's recall ranking.
    Blame(BlameArgs),
    /// Run health checks and print status (dream-cycle mode, etc.).
    Doctor(DoctorArgs),
    /// Run a dream cycle: write LESSONS.md and prune decayed episodic events.
    /// Uses a configured LLM, or the deterministic exemplar when there is no key.
    Dream(DreamArgs),
    /// Remove one episodic event and the lesson state that names it.
    ///
    /// Stop a live `dreamd watch` first; the command refuses while the daemon
    /// is running.
    Forget(ForgetArgs),
    /// Scaffold a new per-project .agent/ store and register it. An existing .agent/ is left unchanged and is not registered.
    ///
    /// Requires a project-root sentinel in cwd or an ancestor.
    Init(InitArgs),
    /// Start the MCP server (bridges to daemon if running, otherwise in-process).
    Mcp(McpArgs),
    /// Name memory snapshots as branches and check them out.
    ///
    /// Refs live under .agent/.dreamd/branches/. Checkout replaces the live
    /// memory files and refuses while `dreamd watch` is running.
    Memory(MemoryArgs),
    /// Migrate episodic schema versions. The only path is 1.0.0 to 1.0.0, a no-op.
    Migrate(MigrateArgs),
    /// Search stored memories by query, ranked by relevance and importance (markdown table).
    Recall(RecallArgs),
    /// List the top-N stored memories by importance, without a search query.
    ///
    /// Uses Tantivy AllQuery (score 1.0 per doc), so printed rows show
    /// `bm25 ≈ 1` and `score ≈ salience` — expected SalienceCollector behavior.
    Score(ScoreArgs),
    /// Report the salience distribution, top clusters, and drift against the snapshot from 7 days ago.
    ///
    /// Scores every episodic doc with salience alone (no BM25) and writes
    /// today's snapshot under `.agent/.dreamd/observability/`.
    SalienceDrift(SalienceDriftArgs),
    /// Reset scratch state. Today only `workspace` is supported.
    Reset(ResetArgs),
    /// Manage the per-user service that runs `dreamd watch` (Linux systemd --user unit / macOS LaunchAgent / Windows scheduled task).
    // Nested so uninstall can land later without reshuffling
    // top-level parsing (same shape as `reset`).
    Service(ServiceArgs),
    /// Front door: scaffold .agent/ (via init) and print the harness wiring next steps.
    Setup(SetupArgs),
    /// Print daemon liveness, resolved project, last dream cycle, and recent log.
    Status,
    /// Stop local dreamd servers, unregister this project, and clear caches.
    Uninstall(UninstallArgs),
    /// Clear the native binary cache so `npx -y dreamd-mcp` fetches latest.
    Update(UpdateArgs),
    /// Run the daemon in foreground mode. Blocks until SIGINT/SIGTERM.
    Watch(WatchArgs),
    /// Download the optional embedding model. Recall is unchanged.
    ///
    /// The default binary is built without the `vectors` feature: `enable`
    /// says so and exits 2. A build with `--features vectors` downloads
    /// BAAI/bge-small-en-v1.5 into the daemon home (~/.agent/models).
    Vectors(VectorsArgs),
    /// Print structured version information (semver, commit, build date, target, schema).
    Version,
}

/// Args for `dreamd archive`. Force-unpin is the only operation today; the
/// struct is shaped to grow (future maintenance ops) without reshuffling
/// top-level parsing. Target-mode invariants (`--force-unpin` required; exactly
/// one of `<EVENT_ID>` / `--all`) are enforced at runtime in
/// [`commands::archive::run`] so they surface a clear message and non-zero exit.
#[derive(Args)]
pub struct ArchiveArgs {
    /// Clear the `pinned` flag so the next pruning pass can remove the entry.
    #[arg(long)]
    pub force_unpin: bool,
    /// The event id to unpin. Mutually exclusive with --all.
    #[arg(value_name = "EVENT_ID")]
    pub id: Option<String>,
    /// Unpin every entry. Refused unless explicitly set.
    #[arg(long)]
    pub all: bool,
}

/// Arguments for the `dreamd blame` subcommand (BZR-193).
///
/// A separate table from `dreamd recall` (`id`, `timestamp`, `skill_action`,
/// `content`, `bm25`, `salience`, `total`, `layer`) at a smaller default `-k`
/// (5, vs recall's 10). `--json` prints one compact JSON array instead.
#[derive(Args)]
pub struct BlameArgs {
    /// Free-text query (BM25 over episodic content).
    pub query: String,
    /// Max results (default 5 — recall stays at 10).
    #[arg(short = 'k', long, default_value_t = 5)]
    pub k: usize,
    /// Print per-hit salience factor breakdown (DR-204).
    #[arg(long)]
    pub explain: bool,
    /// Print one compact JSON array instead of the markdown table.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for the `dreamd init` subcommand.
#[derive(Args)]
pub struct InitArgs {
    /// Suppress non-essential output (state.json, .gitignore, registry, disclosure).
    #[arg(long)]
    pub quiet: bool,
    /// Remove this project's entry from ~/.agent/registry.toml and exit.
    /// Does not delete the project's .agent/ store.
    #[arg(long)]
    pub uninstall_project: bool,
}

/// Arguments for the `dreamd mcp` subcommand.
#[derive(Args)]
pub struct McpArgs {
    /// Hard-lock dream cycle to manual-only mode (overrides config.toml).
    #[arg(long)]
    pub manual_only: bool,
    /// Override the project root used for agent-root discovery.
    /// Required when the IDE launches the MCP server from a non-project CWD
    /// (e.g. Cursor's global ~/.cursor/mcp.json). Must be an absolute path to
    /// the directory that contains `.agent/`.
    #[arg(long, value_name = "PATH")]
    pub project_root: Option<PathBuf>,
    /// Serve MCP over Streamable HTTP at http://ADDR/mcp instead of stdio: an IP
    /// or IP:port, e.g. 127.0.0.1:8080 or [::1]:0. Port defaults to 0
    /// (OS-chosen). Hostnames are not resolved. No auth: any local process can
    /// reach a loopback bind. Needs a build with the `mcp-http` cargo feature
    /// (prebuilt release binaries are stdio-only).
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<String>,
    /// Allow a non-loopback --bind. Logs a warning on every start and accepts
    /// any Host header. Requires --bind.
    #[arg(long)]
    pub insecure: bool,
}

/// Arguments for the `dreamd migrate` subcommand (WEG-133 / DR-108).
///
/// `--from` / `--to` are **episodic record** schema strings
/// (`dreamd_protocol::RECORD_SCHEMA_VERSION`), e.g. `1.0.0` — not the daemon
/// `state.json` schema the `dreamd version` display line prints, and not the
/// Tantivy index schema. At v0.1 the only registered path is the identity
/// `1.0.0 → 1.0.0` (a no-op).
#[derive(Args)]
pub struct MigrateArgs {
    /// Source episodic record schema version (e.g. `1.0.0`).
    #[arg(long)]
    pub from: String,
    /// Target episodic record schema version (e.g. `1.0.0`).
    #[arg(long)]
    pub to: String,
}

/// Arguments for the `dreamd recall` subcommand (WEG-51 / DR-703).
#[derive(Args)]
pub struct RecallArgs {
    /// Free-text query (BM25 over episodic content).
    pub query: String,
    /// Max results (default 10 — CLI demo default; HTTP/MCP stay at 5).
    #[arg(short = 'k', long, default_value_t = 10)]
    pub k: usize,
    /// Print per-hit salience factor breakdown (DR-204).
    #[arg(long)]
    pub explain: bool,
    /// Drop these event ids before top-k. Repeatable. Does not modify the index.
    #[arg(long = "without", action = ArgAction::Append)]
    pub without: Vec<String>,
    /// Drop hits whose skill_action equals this prefix or extends it with `::`.
    #[arg(long = "without-cluster", action = ArgAction::Append)]
    pub without_cluster: Vec<String>,
}

/// Arguments for the `dreamd score` subcommand (WEG-52 / DR-704).
///
/// Ranks the index by query-time salience with no lexical BM25 filter
/// (`AllQuery`). Under that path every hit has `bm25 ≈ 1` and
/// `score ≈ salience` — expected Tantivy/`SalienceCollector` behavior.
#[derive(Args)]
pub struct ScoreArgs {
    /// Max results (default 100).
    #[arg(short = 'n', long, default_value_t = 100)]
    pub n: usize,
    /// Print per-hit salience factor breakdown (same blocks as `dreamd recall --explain`).
    #[arg(long)]
    pub explain: bool,
}

/// Args for `dreamd salience-drift` (BZR-195).
#[derive(Args)]
pub struct SalienceDriftArgs {
    /// Print one compact JSON report instead of the text lines.
    #[arg(long)]
    pub json: bool,
}

/// Args for `dreamd memory` (BZR-150). Nested like [`ServiceArgs`]. Format is
/// `docs/branching.md` (`branches/1.0`).
#[derive(Args)]
pub struct MemoryArgs {
    #[command(subcommand)]
    pub command: MemoryCommand,
}

#[derive(Subcommand)]
pub enum MemoryCommand {
    /// Snapshot the live memory files, name the snapshot <NAME>, and make it current.
    ///
    /// Fails if the branch already exists. Prints the object id.
    Branch {
        /// Branch name: [a-z0-9][a-z0-9._-]{0,63}, no `..`.
        name: String,
    },
    /// Replace the live memory files with branch <NAME> and make it current.
    ///
    /// Refuses while the daemon socket exists; stop `dreamd watch` first.
    /// Prints the object id.
    Checkout {
        /// Branch name to check out.
        name: String,
    },
    /// List branches; `*` marks the current one.
    Branches,
    /// Delete branch <NAME>. The snapshot object stays on disk.
    ///
    /// Refuses to delete the current branch.
    Delete {
        /// Branch name to delete.
        name: String,
    },
    /// Binary-search the snap-* timeline for the first bad snapshot (BZR-153).
    ///
    /// Searches refs by the timestamp on their second line; there is no parent
    /// pointer. Each step checks out a snapshot, so it refuses while `dreamd
    /// watch` is running. Prints `<name> <id>`.
    Bisect(BisectArgs),
    /// Compare two snapshots: added/removed events, salience changes, file verdicts.
    ///
    /// Reads objects only; never checks out. Each side is a ref name,
    /// `<name>:<id>` (the ref must name that id), or a 64-hex object id.
    Diff {
        /// Older side: ref name, `<name>:<id>`, or 64-hex object id.
        from: String,
        /// Newer side: ref name, `<name>:<id>`, or 64-hex object id.
        to: String,
        /// When LESSONS.md is modified, dump both versions (`-` from, `+` to).
        #[arg(long)]
        unified: bool,
        /// Print one compact JSON object instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// Args for `dreamd memory bisect` (BZR-153). State is
/// `.agent/.dreamd/branches/bisect`, which is not a ref.
#[derive(Args)]
pub struct BisectArgs {
    #[command(subcommand)]
    pub command: BisectCommand,
}

#[derive(Subcommand)]
pub enum BisectCommand {
    /// Start a bisect between a good and a bad ref, and check out the midpoint.
    ///
    /// Endpoints are ref names or object ids a ref names. The good ref's
    /// timestamp must be strictly earlier than the bad ref's. Adjacent
    /// endpoints print the bad ref and start nothing.
    Start {
        /// Last known good ref (name or 64-hex object id).
        #[arg(long)]
        good: String,
        /// First known bad ref (name or 64-hex object id).
        #[arg(long)]
        bad: String,
        /// Run SCRIPT with `sh -c` after each checkout and mark by its exit
        /// status (0 = good) until the first bad ref is found.
        #[arg(long, value_name = "SCRIPT")]
        auto_test: Option<String>,
    },
    /// Mark the checked-out snapshot good and check out the next midpoint.
    Good,
    /// Mark the checked-out snapshot bad and check out the next midpoint.
    Bad,
    /// Run SCRIPT once with `sh -c` and mark good on exit 0, bad otherwise.
    ///
    /// Requires a bisect in progress.
    Run {
        /// Shell command string, passed to `sh -c`.
        script: String,
    },
}

/// Args for `dreamd reset`. Wraps the nested target subcommand so the shape
/// scales when more reset targets land (e.g., personal, episodic) without
/// reshuffling top-level command parsing.
#[derive(Args)]
pub struct ResetArgs {
    #[command(subcommand)]
    pub command: ResetCommand,
}

#[derive(Subcommand)]
pub enum ResetCommand {
    /// Clear working/WORKSPACE.md back to its initial scaffold contents.
    Workspace {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

/// Args for `dreamd service`. Wraps the nested verb so the lifecycle set
/// (install / start / restart / status / uninstall — the last landed with
/// AILAB-202) grew without reshuffling top-level command parsing. Same shape
/// as [`ResetArgs`]. The nested `uninstall` is not the top-level
/// [`Command::Uninstall`] (AILAB-226), which stops processes and clears
/// caches rather than removing the OS service entry.
#[derive(Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Write a user-scope service for `dreamd watch` and enable it now
    /// (Linux systemd --user / macOS LaunchAgent / Windows scheduled task).
    Install {
        /// Overwrite an existing LaunchAgent plist (macOS) or scheduled-task XML (Windows). No effect on Linux.
        #[arg(long)]
        force: bool,
    },
    /// Start the user-scope service (systemctl --user start / launchctl kickstart).
    Start,
    /// Bounce the user-scope service (systemctl --user restart / launchctl kickstart -k).
    Restart,
    /// Report whether the user-scope service is running (systemd / launchd).
    /// Daemon-level liveness (UDS) is `dreamd status`.
    Status,
    /// Remove the user-scope service. Never deletes per-project .agent/ stores.
    ///
    /// Tears down the OS service entry only: the systemd --user unit
    /// (`disable --now`, remove the unit file, `daemon-reload`) or the macOS
    /// LaunchAgent (`launchctl bootout`, remove the plist). It is idempotent —
    /// a unit or plist that is already gone is not an error. By default no
    /// data is deleted at all: the daemon home `~/.agent/` and every
    /// per-project `.agent/` store are left exactly as they are, and both are
    /// reported on the `preserved:` lines.
    ///
    /// `--purge` additionally deletes the daemon home `~/.agent/` — the
    /// registry, the socket and `dreamd.log`, and nothing else. It never walks
    /// or removes a per-project `<repo>/.agent/` store. It needs `--yes` or a
    /// typed `y` on a tty, like `dreamd reset workspace`.
    ///
    /// This is not `dreamd uninstall`, which stops running processes, drops
    /// the socket and clears caches without touching the OS service entry.
    Uninstall {
        /// Also remove ~/.agent/ (registry, socket, log). Not per-project stores.
        #[arg(long)]
        purge: bool,
        /// Confirm --purge without a prompt (`reset workspace --yes`).
        #[arg(long)]
        yes: bool,
    },
}

/// Args for `dreamd vectors` (BZR-181). Nested like [`ServiceArgs`].
#[derive(Args)]
pub struct VectorsArgs {
    #[command(subcommand)]
    pub command: VectorsCommand,
}

#[derive(Subcommand)]
pub enum VectorsCommand {
    /// Download BAAI/bge-small-en-v1.5 into ~/.agent/models.
    ///
    /// Works only in a binary built with `--features vectors`. The default
    /// binary was built without it: this prints that and exits 2, and creates
    /// no directory. When `HF_HOME` is set, the download goes there instead.
    Enable,
}

/// Arguments for the `dreamd setup` subcommand (AILAB-549 / AILAB-550).
///
/// `setup` is the install front door: it scaffolds `.agent/`, merges the
/// `mcpServers.dreamd` block into the harness config, and prints the verify
/// step. On a TTY without `--yes` the AILAB-551 wizard fills in the answers the
/// user did not already type as flags; the clap surface itself is unchanged.
#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Accept defaults without prompting. Required when stdin is not a tty.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Harness(es) to wire up: `claude` writes .mcp.json, `cursor` writes
    /// .cursor/mcp.json, `both` writes both, `none` writes neither.
    #[arg(long, value_enum, default_value_t = commands::setup::Harness::Both)]
    pub harness: commands::setup::Harness,
    /// Do not write any harness MCP config.
    #[arg(long)]
    pub no_write_mcp: bool,
    /// Replace an existing `mcpServers.dreamd` entry that does not run
    /// `npx -y dreamd-mcp`. Other MCP servers are preserved. Does not override
    /// a config file that is not valid JSON.
    #[arg(long)]
    pub force: bool,
    /// Start the shared daemon in the background after scaffolding.
    #[arg(long, overrides_with = "no_start_watch")]
    pub start_watch: bool,
    /// Only print the `dreamd watch` command instead of starting it (default).
    #[arg(long, overrides_with = "start_watch")]
    pub no_start_watch: bool,
    /// Print the planned actions without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

impl SetupArgs {
    /// Resolve the `--start-watch` / `--no-start-watch` pair. Both flags
    /// override each other in clap, so the last one on the command line wins.
    pub fn wants_start_watch(&self) -> bool {
        self.start_watch && !self.no_start_watch
    }
}

/// Arguments for the `dreamd uninstall` subcommand (AILAB-226).
#[derive(Args)]
pub struct UninstallArgs {
    /// Keep the native binary cache (~/.cache/dreamd-mcp) and npx cache entries.
    #[arg(long)]
    pub keep_caches: bool,
    /// Wipe the ENTIRE npx cache (every cached package, not just dreamd-mcp).
    /// Prints a warning before deleting.
    #[arg(long)]
    pub all_npx: bool,
    /// Suppress non-essential output (errors still print to stderr).
    #[arg(short = 'q', long)]
    pub quiet: bool,
}

/// Arguments for the `dreamd update` subcommand (AILAB-226).
#[derive(Args)]
pub struct UpdateArgs {
    /// Print the current version and planned actions without changing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Suppress non-essential output (errors still print to stderr).
    #[arg(short = 'q', long)]
    pub quiet: bool,
    /// Explicitly stop local `dreamd mcp` / `dreamd watch` (already the default; nothing is relaunched for you).
    #[arg(long)]
    pub restart: bool,
}

/// Render the `dreamd(1)` man page from the clap definition.
///
/// Used by the `generate_man` bin; exposed so tests can assert the page
/// renders without writing `docs/dreamd.1`.
pub fn render_man_page() -> std::io::Result<Vec<u8>> {
    use clap::CommandFactory;
    use clap_mangen::Man;

    let man = Man::new(Cli::command());
    let mut buffer = Vec::new();
    man.render(&mut buffer)?;
    Ok(buffer)
}

/// Guard that rejects `Auto` dream-cycle mode at v0.1.
///
/// Returns `Ok(())` for `Manual` mode; `Err(message)` for `Auto` mode.
/// The caller is responsible for printing the error and calling
/// `std::process::exit(1)` — this function does not exit so it can be
/// unit-tested without spawning a subprocess.
pub fn check_dream_mode(config: &Config) -> Result<(), String> {
    if config.dream_cycle_mode == DreamCycleMode::Auto {
        return Err("dream_cycle_mode = auto is not supported; \
             set dream_cycle_mode = \"manual\" in config.toml. \
             Cycles run only when you invoke them."
            .to_string());
    }
    Ok(())
}

/// Resolve the existing `.agent/` store for a post-init command, walking up
/// from `cwd`. On a miss, print a pointer to `dreamd init` (stderr) and return
/// the exit code the caller must propagate.
///
/// This is the *find* resolver (`AgentRoot::discover`), shared by `dream` and
/// `doctor`. `init` uses its own sentinel walk because it *creates* a store;
/// these commands *find* one. Walking up means `dreamd dream` from a project
/// subdirectory operates on the project's store, and an uninitialized dir
/// errors instead of silently scaffolding an empty `.agent/`.
fn discover_store_or_exit(
    cwd: &std::path::Path,
) -> Result<dreamd_core::layout::AgentRoot, ExitCode> {
    dreamd_core::layout::AgentRoot::discover(cwd).map_err(|_| {
        eprintln!(
            "dreamd: no .agent/ store found in {} or any parent directory — run `dreamd init` first.",
            cwd.display()
        );
        ExitCode::from(2)
    })
}

fn current_dir_or_exit() -> Result<std::path::PathBuf, ExitCode> {
    std::env::current_dir().map_err(|e| {
        eprintln!("dreamd: error — could not read current directory: {e}");
        ExitCode::from(1)
    })
}

/// This invocation's home directory, or `None`. Delegates to
/// [`dreamd_core::layout::home_dir`], the one place dreamd reads `HOME` (and,
/// on Windows, `USERPROFILE`); the rules — empty is unset, `USERPROFILE` only
/// on Windows — live on [`dreamd_core::layout::resolve_home_from_vars`].
fn home_dir() -> Option<PathBuf> {
    dreamd_core::layout::home_dir()
}

/// The `dreamd service` refusal copy for a host with no usable home
/// directory. Windows names both variables because
/// [`dreamd_core::layout::resolve_home_from_vars`] consults both there; Unix keeps the wording it
/// has always had, since `USERPROFILE` is never read on that side and naming
/// it would only send the reader after a variable that could not have helped.
/// The three `service` verbs use this, and so do `init`, `setup`, and
/// `uninstall`, which exit 2 without a daemon home rather than fall back to a
/// relative `.agent` (BZR-168). Every other `home_dir` caller has its own
/// "skip with a note" behaviour and its own wording.
fn no_home_message() -> &'static str {
    if cfg!(windows) {
        "neither HOME nor USERPROFILE is set; cannot locate the per-user service path"
    } else {
        "HOME is not set; cannot locate the per-user service path"
    }
}

/// Which subcommands own the shared daemon log at `~/.agent/dreamd.log`.
///
/// Only the daemon does. `init_tracing` opens the path with `truncate(true)`,
/// so any short-lived invocation that got a path would wipe a running daemon's
/// accumulated log — and, `archive` aside, no CLI one-shot emits a single
/// `tracing` event into it anyway. `mcp` is long-running but is deliberately
/// excluded: IDEs spawn several concurrently, so a file layer there would have
/// two MCP servers truncating each other's log; its stderr already lands in the
/// IDE's own MCP log. Everything else runs console-only on stderr. AILAB-184.
fn wants_daemon_log(command: Option<&Command>) -> bool {
    matches!(command, Some(Command::Watch(_)))
}

/// `~/.agent`, or `None` when there is no home directory. There is no
/// relative fallback: a bare `.agent` is the *project* store under cwd, not
/// the daemon home (BZR-168).
fn resolve_daemon_home() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".agent"))
}

fn run_archive(args: ArchiveArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    // Resolve the daemon socket the same way `status` does ($DREAMD_SOCK
    // else ~/.agent/dreamd.sock). The guard refuses if it probes live so
    // a running daemon can't clobber the rewrite.
    #[cfg(unix)]
    let socket = dreamd_core::client::resolve_daemon_socket();
    // Off Unix the daemon is loopback TCP, so the address the guard probes is
    // `~/.agent/server.json` (AILAB-192), resolved off `$HOME` through the same
    // one `home_dir()` helper the registry and log paths use.
    // `archive::daemon_liveness` turns it into a real TCP connect.
    #[cfg(not(unix))]
    let socket: Option<PathBuf> =
        home_dir().map(|h| dreamd_core::layout::DaemonHome::new(h.join(".agent")).server_json());
    // lock-ok (AILAB-583): archive never opens a Tantivy index — it only reads
    // and rewrites the episodic JSONL. The liveness probe below does spawn a
    // thread and block on it, but that closure only connects to the socket —
    // it emits no tracing, and the wait is bounded by LIVENESS_PROBE_TIMEOUT —
    // so it cannot wait on a thread that wants the stderr lock we hold.
    // See AGENTS.md no-hoisted-stdio-lock-across-tantivy.
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    match commands::archive::run(
        &cwd,
        socket.as_deref(),
        args.force_unpin,
        args.id.as_deref(),
        args.all,
        &mut out,
        &mut err,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        // Usage / precondition errors — the user must fix the invocation.
        Err(commands::archive::ArchiveError::NotFound)
        | Err(commands::archive::ArchiveError::ForceUnpinRequired)
        | Err(commands::archive::ArchiveError::NoTarget)
        | Err(commands::archive::ArchiveError::IdAndAll) => ExitCode::from(2),
        // Runtime failures — the store/log or a live daemon got in the way.
        Err(commands::archive::ArchiveError::DaemonRunning)
        | Err(commands::archive::ArchiveError::UnknownId(_)) => ExitCode::from(1),
        Err(commands::archive::ArchiveError::Episodic(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
        Err(commands::archive::ArchiveError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_blame(args: BlameArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    // Query instant: wall clock, same caveat as `dreamd recall` (not byte-stable,
    // no SOURCE_DATE_EPOCH override).
    let now_sec = chrono::Utc::now().timestamp();
    // Unlocked handles (AILAB-583): blame opens the Tantivy index the same way
    // recall does; see the comment on `run_recall`.
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    match commands::blame::run(
        &cwd,
        &args.query,
        args.k,
        args.explain,
        args.json,
        now_sec,
        &mut out,
        &mut err,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        // Usage / precondition — the user must fix the invocation or init first.
        Err(commands::recall::RecallError::NotFound) => ExitCode::from(2),
        // Runtime — the index is not ready, or a search/IO failure.
        Err(commands::recall::RecallError::IndexUnavailable(_)) => ExitCode::from(1),
        Err(commands::recall::RecallError::Search(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
        Err(commands::recall::RecallError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_doctor(args: DoctorArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let config = match load_config(&cwd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dreamd: error — {e}");
            return ExitCode::from(1);
        }
    };
    let agent_root = match discover_store_or_exit(&cwd) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let skip = dreamd_core::autobiography::read_last_skip(&agent_root);
    // Socket via the shared resolver ($DREAMD_SOCK else the daemon-home
    // socket), same as the status arm — never a hardcoded path.
    #[cfg(unix)]
    let socket = dreamd_core::client::resolve_daemon_socket();
    #[cfg(not(unix))]
    let socket: Option<PathBuf> = None;
    // Unlocked handles, deliberately — do NOT hoist a `.lock()` out here.
    // `--repair` reopens the index, and Tantivy's `segment_updater` thread logs
    // through the tracing console layer, which writes to `std::io::stderr`.
    // `StdoutLock`/`StderrLock` are process-wide and reentrant only for the
    // owning thread, so holding one across the rebuild deadlocks: the updater
    // thread blocks writing its log line while this thread blocks on that same
    // updater's commit, and `dreamd doctor --repair` never exits
    // (AILAB-575 / AILAB-561). Each `writeln!` below locks and releases on its
    // own, so no lock is ever held across `TantivyIndexHandle::open`.
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let flags = commands::doctor::DoctorFlags {
        repair: args.repair,
        cluster_health: args.cluster_health,
        provenance: args.provenance,
    };
    let now_sec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    match commands::doctor::run(
        &config,
        &agent_root,
        skip.as_ref(),
        socket.as_deref(),
        flags,
        now_sec,
        &mut out,
        &mut err,
    ) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_forget(args: ForgetArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    // Same daemon address as `run_archive`: the UDS socket on Unix,
    // `~/.agent/server.json` off it.
    #[cfg(unix)]
    let socket = dreamd_core::client::resolve_daemon_socket();
    #[cfg(not(unix))]
    let socket: Option<PathBuf> =
        home_dir().map(|h| dreamd_core::layout::DaemonHome::new(h.join(".agent")).server_json());
    let now_sec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    // Unlocked handles (AILAB-583): after a removal the write path opens the
    // Tantivy index, whose writer thread logs to stderr. A hoisted lock would
    // deadlock the same way `run_doctor` did. See AGENTS.md
    // no-hoisted-stdio-lock-across-tantivy.
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    match commands::forget::run(
        &cwd,
        socket.as_deref(),
        &args.id,
        args.dry_run,
        args.proof.as_deref(),
        now_sec,
        &mut out,
        &mut err,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        // Usage / precondition — `run` already printed the hint.
        Err(commands::forget::ForgetCliError::NotFound)
        | Err(commands::forget::ForgetCliError::BadId(_)) => ExitCode::from(2),
        Err(e) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_dream(args: DreamArgs) -> ExitCode {
    if let Err(msg) = args.validate() {
        eprintln!("dreamd: {msg}");
        return ExitCode::from(2);
    }
    if args.auto {
        eprintln!(
            "dreamd: --auto is not supported. \
             Set dream_cycle_mode = \"manual\" in config.toml. \
             Cycles run only when you invoke them."
        );
        return ExitCode::from(2);
    }
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let config = match load_config(&cwd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dreamd: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(msg) = check_dream_mode(&config) {
        eprintln!("dreamd: {msg}");
        return ExitCode::from(2);
    }
    let agent_root = match discover_store_or_exit(&cwd) {
        Ok(r) => r,
        Err(code) => return code,
    };
    // AILAB-341: `--dry` writes nothing, so it forks off here — after config and
    // store resolution (a preview still needs to know which store it is
    // previewing), before the write path. It deliberately does NOT proxy to the
    // daemon: `POST /api/v1/dream` performs the mutation the flag exists to
    // avoid. `--no-commit` is a no-op alongside it, not an error.
    if args.dry {
        // lock-ok (AILAB-583 / AILAB-204): stdout stays UNLOCKED — the preview
        // may sit on the model call for up to 90s.
        // See AGENTS.md no-hoisted-stdio-lock-across-tantivy.
        let outcome = commands::dream::run_dry(
            agent_root.project_root(),
            &mut std::io::stdout(),
            args.no_llm,
            // AILAB-199: `--dry` skips the proxy, so consent cannot arrive as a
            // header on this path and is taken in-process instead.
            args.share_personal,
        );
        return match outcome {
            // Banner on stderr so stdout is exactly the would-be file bytes.
            Ok(commands::dream::DryOutcome::WouldWrite) => {
                eprintln!("dreamd: dry run; would write .agent/semantic/LESSONS.md (not written)");
                ExitCode::SUCCESS
            }
            Ok(commands::dream::DryOutcome::WouldRetire) => {
                eprintln!("dreamd: dry run; would retire LESSONS.md (not written)");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("dreamd: {e}");
                ExitCode::from(1)
            }
        };
    }

    // lock-ok (AILAB-583 / AILAB-204): `&mut std::io::stdout()` stays UNLOCKED.
    // The cycle opens a Tantivy index and — since AILAB-204 — may sit on a
    // network call for up to 90s; holding a std stream lock across either is the
    // documented deadlock. See AGENTS.md no-hoisted-stdio-lock-across-tantivy.
    match commands::dream::run(
        agent_root.project_root(),
        &mut std::io::stdout(),
        args.no_commit,
        args.no_llm,
        args.share_personal,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dreamd: {e}");
            ExitCode::from(1)
        }
    }
}

fn run_init(args: InitArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let Some(daemon_home) = resolve_daemon_home() else {
        eprintln!("dreamd: error — {}", no_home_message());
        return ExitCode::from(2);
    };
    let daemon_home = dreamd_core::layout::DaemonHome::new(daemon_home);
    // lock-ok (AILAB-583): init never opens a Tantivy index — it scaffolds or
    // removes `.agent/` files and the registry entry.
    // See AGENTS.md no-hoisted-stdio-lock-across-tantivy.
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let result = if args.uninstall_project {
        commands::init::uninstall_project(&cwd, &daemon_home, args.quiet, &mut out, &mut err)
    } else {
        commands::init::run(&cwd, &daemon_home, args.quiet, &mut out, &mut err)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(commands::init::InitError::NoProjectRoot) => ExitCode::from(2),
        Err(commands::init::InitError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_mcp(args: McpArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    // --project-root overrides CWD-based agent-root discovery. Required when
    // an IDE launches the MCP server from a non-project CWD.
    let effective_root = match args.project_root.clone() {
        Some(p) => p,
        None => cwd,
    };
    let mut config = match load_config(&effective_root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dreamd: error — {e}");
            return ExitCode::from(1);
        }
    };
    // --manual-only overrides config before the Auto guard runs.
    if args.manual_only {
        config.dream_cycle_mode = DreamCycleMode::Manual;
    }
    if let Err(msg) = check_dream_mode(&config) {
        eprintln!("dreamd: error — {msg}");
        std::process::exit(1);
    }
    commands::mcp::run(&effective_root, &args)
}

fn run_migrate(args: MigrateArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    // lock-ok (AILAB-583): migrate never opens a Tantivy index — it reads the
    // index manifest JSON only (`dreamd_core::index::check_manifest_version`).
    // See AGENTS.md no-hoisted-stdio-lock-across-tantivy.
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    match commands::migrate::run(&cwd, &args.from, &args.to, &mut out, &mut err) {
        Ok(()) => ExitCode::SUCCESS,
        // Usage — no store to migrate; run `dreamd init` first.
        Err(commands::migrate::MigrateError::NotFound) => ExitCode::from(2),
        // Runtime — unregistered path (already reported inside run).
        Err(commands::migrate::MigrateError::NoMigration { .. }) => ExitCode::from(1),
        // Runtime — a registered transform or a `.bak`/IO write failed.
        Err(commands::migrate::MigrateError::Apply(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
        Err(commands::migrate::MigrateError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_recall(args: RecallArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    // Query instant: wall clock. Recall derives age_days / salience decay
    // live from this, so — unlike `dreamd dream` — it is not byte-stable
    // and takes no SOURCE_DATE_EPOCH override.
    let now_sec = chrono::Utc::now().timestamp();
    // Unlocked handles (AILAB-583): recall opens the Tantivy index (read-only
    // `Index::open_in_dir` + `reader()`, whose reload policy owns a background
    // watcher thread). No writer today, but per AGENTS.md
    // `no-hoisted-stdio-lock-across-tantivy` an index-opening arm never holds a
    // hoisted lock that a logging index thread could deadlock on (AILAB-575).
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    // Counterfactual recall (BZR-194): drop before top-k, index untouched.
    let exclusion = RecallExclusion {
        ids: args.without,
        cluster_prefixes: args.without_cluster,
    };
    match commands::recall::run(
        &cwd,
        &args.query,
        args.k,
        args.explain,
        &exclusion,
        now_sec,
        &mut out,
        &mut err,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        // Usage / precondition — the user must fix the invocation or init first.
        Err(commands::recall::RecallError::NotFound) => ExitCode::from(2),
        // Runtime — the index is not ready, or a search/IO failure.
        Err(commands::recall::RecallError::IndexUnavailable(_)) => ExitCode::from(1),
        Err(commands::recall::RecallError::Search(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
        Err(commands::recall::RecallError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_score(args: ScoreArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    // Query instant: wall clock (same as recall — age_days / salience decay).
    let now_sec = chrono::Utc::now().timestamp();
    // Unlocked handles (AILAB-583): score opens the Tantivy index the same way
    // recall does (read-only `Index::open_in_dir` + `reader()`, background
    // watcher thread). Per AGENTS.md `no-hoisted-stdio-lock-across-tantivy` an
    // index-opening arm never holds a hoisted lock that a logging index thread
    // could deadlock on (AILAB-575).
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    match commands::score::run(&cwd, args.n, args.explain, now_sec, &mut out, &mut err) {
        Ok(()) => ExitCode::SUCCESS,
        Err(commands::score::ScoreError::NotFound) => ExitCode::from(2),
        Err(commands::score::ScoreError::IndexUnavailable(_)) => ExitCode::from(1),
        Err(commands::score::ScoreError::Search(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
        Err(commands::score::ScoreError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_salience_drift(args: SalienceDriftArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let now_sec = chrono::Utc::now().timestamp();
    // Unlocked handles (AILAB-583): opens the Tantivy index read-only, same as
    // `run_score` (`no-hoisted-stdio-lock-across-tantivy`).
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    match commands::salience::run(&cwd, args.json, now_sec, &mut out, &mut err) {
        Ok(()) => ExitCode::SUCCESS,
        Err(commands::salience::SalienceError::NotFound) => ExitCode::from(2),
        Err(commands::salience::SalienceError::IndexUnavailable(_)) => ExitCode::from(1),
        Err(commands::salience::SalienceError::Report(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
        Err(commands::salience::SalienceError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

/// The daemon UDS path checkout refuses on, when a home directory resolves.
fn daemon_socket() -> Option<PathBuf> {
    home_dir().map(|h| dreamd_core::layout::DaemonHome::new(h.join(".agent")).socket_path())
}

/// `dreamd memory …` (BZR-150, bisect BZR-153, diff BZR-156). Never opens a Tantivy index;
/// stdout stays unlocked per call. Exit 2 when no `.agent/` store is found, 1
/// on any snapshot or bisect error (including the daemon-socket refusal on
/// checkout and a bisect script `sh` cannot spawn).
fn run_memory(command: MemoryCommand) -> ExitCode {
    use commands::memory::{self, MemoryError};
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let mut out = std::io::stdout();
    let result = match command {
        MemoryCommand::Branch { name } => {
            memory::run_branch(&cwd, &name, chrono::Utc::now().timestamp(), &mut out)
        }
        MemoryCommand::Checkout { name } => {
            memory::run_checkout(&cwd, &name, daemon_socket().as_deref(), &mut out)
        }
        MemoryCommand::Branches => memory::run_branches(&cwd, &mut out),
        MemoryCommand::Delete { name } => memory::run_delete(&cwd, &name),
        MemoryCommand::Bisect(BisectArgs { command }) => {
            let socket = daemon_socket();
            match command {
                BisectCommand::Start {
                    good,
                    bad,
                    auto_test,
                } => memory::run_bisect_start(
                    &cwd,
                    &good,
                    &bad,
                    auto_test.as_deref(),
                    socket.as_deref(),
                    &mut out,
                ),
                BisectCommand::Good => {
                    memory::run_bisect_mark(&cwd, true, socket.as_deref(), &mut out)
                }
                BisectCommand::Bad => {
                    memory::run_bisect_mark(&cwd, false, socket.as_deref(), &mut out)
                }
                BisectCommand::Run { script } => {
                    memory::run_bisect_run(&cwd, &script, socket.as_deref(), &mut out)
                }
            }
        }
        MemoryCommand::Diff {
            from,
            to,
            unified,
            json,
        } => memory::run_diff(
            &cwd,
            &from,
            &to,
            unified,
            json,
            chrono::Utc::now().timestamp(),
            &mut out,
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e @ MemoryError::NotFound) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_reset_workspace(yes: bool) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    // lock-ok (AILAB-583): reset never opens a Tantivy index — it rewrites
    // `working/WORKSPACE.md` on disk. Note the confirm prompt holds these locks
    // across a blocking stdin read: safe only while nothing under reset logs
    // from another thread, so keep it index-free.
    // See AGENTS.md no-hoisted-stdio-lock-across-tantivy.
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    match commands::reset::run_workspace(&cwd, yes, &mut out, &mut err) {
        Ok(()) => ExitCode::SUCCESS,
        Err(commands::reset::ResetError::NotFound)
        | Err(commands::reset::ResetError::NotATty)
        | Err(commands::reset::ResetError::Declined) => ExitCode::from(2),
        Err(commands::reset::ResetError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

/// Was this arg typed on the command line, as opposed to filled in by clap's
/// default? The wizard skips a prompt only for a flag the user actually typed
/// (AILAB-551 §1.7) — comparing against the default would wrongly treat
/// `--harness both` as "unanswered".
fn from_command_line(m: &clap::ArgMatches, id: &str) -> bool {
    matches!(
        m.value_source(id),
        Some(clap::parser::ValueSource::CommandLine)
    )
}

/// Which `setup` prompts survive the flags the user already typed.
///
/// `--no-write-mcp` has no prompt of its own — the harness prompt's option 4
/// covers "skip" — so it only needs to be honoured, which `write_mcp` already
/// does. Without matches (unreachable from the `Setup` arm) we prompt for
/// nothing, since we cannot tell an explicit flag from a default.
fn setup_interactive(m: Option<&clap::ArgMatches>) -> commands::setup::Interactive {
    let Some(m) = m else {
        return commands::setup::Interactive::default();
    };
    commands::setup::Interactive {
        ask_harness: !from_command_line(m, "harness"),
        ask_watch: !(from_command_line(m, "start_watch") || from_command_line(m, "no_start_watch")),
    }
}

fn run_setup(args: SetupArgs, interactive: commands::setup::Interactive) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let Some(daemon_home) = resolve_daemon_home() else {
        eprintln!("dreamd: error — {}", no_home_message());
        return ExitCode::from(2);
    };
    // Unlocked handles (AILAB-583): the success-path doctor beat calls into
    // `doctor::run` (`setup::report_doctor`), which is writer-free only while
    // `repair: false`. Doctor buffers its own output, but `StderrLock` is
    // process-wide — a hoisted lock here would still deadlock against Tantivy's
    // logging threads if any check under setup ever opens the index (AILAB-575).
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let req = commands::setup::SetupRequest {
        cwd: &cwd,
        daemon_home: &daemon_home,
        harness: args.harness,
        write_mcp: !args.no_write_mcp,
        force: args.force,
        start_watch: args.wants_start_watch(),
        dry_run: args.dry_run,
        yes: args.yes,
        interactive,
    };
    match commands::setup::run(&req, &mut out, &mut err) {
        Ok(()) => ExitCode::SUCCESS,
        // Usage — no project root to set up, or nowhere to ask the questions
        // (both messages are already on stderr).
        Err(commands::setup::SetupError::NoProjectRoot)
        | Err(commands::setup::SetupError::NotATty) => ExitCode::from(2),
        // Conflict / malformed / unwritable MCP config (message already on
        // stderr, with the --force remedy where one exists).
        Err(commands::setup::SetupError::Mcp(_)) => ExitCode::from(1),
        Err(commands::setup::SetupError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_status() -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    // Socket path via the shared resolver ($DREAMD_SOCK else ~/.agent/dreamd.sock);
    // registry and the daemon log both resolve off $HOME via the one `home_dir()`
    // helper. AILAB-184: `status` no longer installs a file layer, so nothing in
    // this process truncates the log — the tail is read here, in-command, rather
    // than pre-read in `run()` before `init_tracing` (the retired WEG-103 dance).
    #[cfg(unix)]
    let socket = dreamd_core::client::resolve_daemon_socket();
    // Off Unix there is no socket to resolve: the daemon publishes a loopback
    // TCP address to `~/.agent/server.json` (AILAB-192), so that is the path
    // `status` reports and probes. Same `home_dir()` helper as the two paths
    // below, so all three agree on which home this invocation is reading.
    #[cfg(not(unix))]
    let socket: Option<PathBuf> =
        home_dir().map(|h| dreamd_core::layout::DaemonHome::new(h.join(".agent")).server_json());
    let registry_path =
        home_dir().map(|h| dreamd_core::layout::DaemonHome::new(h.join(".agent")).registry_toml());
    let log_tail = home_dir()
        .map(|h| dreamd_core::layout::DaemonHome::new(h.join(".agent")).log_file())
        .map(|p| commands::status::read_log_tail(&p))
        .unwrap_or_default();
    // lock-ok (AILAB-583): status never opens a Tantivy index — it probes the
    // daemon socket and reads registry.toml, the WAL, and the daemon log tail.
    // See AGENTS.md no-hoisted-stdio-lock-across-tantivy.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match commands::status::run(
        &cwd,
        socket.as_deref(),
        registry_path.as_deref(),
        &log_tail,
        &mut out,
    ) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

/// Native binary cache tree used by the npm shim: `~/.cache/dreamd-mcp`
/// (parent of the per-version dirs; clearing it drops all versions).
/// Resolved off [`home_dir`] the same way `resolve_daemon_home` is. `None` — no
/// home directory; the commands print a skip note instead.
fn resolve_shim_cache_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".cache").join("dreamd-mcp"))
}

/// This invocation's `$HOME`, used as the process-stop attribution scope for
/// `uninstall` / `update` (AILAB-584): a running `dreamd mcp` / `dreamd watch`
/// is only ours to signal if its own `HOME=` resolves here. `None` — no home
/// directory; the stop pass then has nothing to attribute against and skips
/// with a warning rather than signalling machine-wide, which is what the bare
/// `pkill -f` it replaced used to do. See [`home_dir`] for why an *empty*
/// `HOME` counts as no home directory here.
fn resolve_home() -> Option<PathBuf> {
    home_dir()
}

/// npx cache root (`~/.npm/_npx`) holding per-package cache entries. `None` —
/// no home directory; `uninstall` skips the npx step rather than scanning (and
/// deleting under) a cwd-relative `./.npm/_npx`.
fn resolve_npx_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".npm").join("_npx"))
}

/// Best-effort cargo-install detection for `dreamd update` messaging: an
/// explicit `DREAMD_BIN` override, or the running executable living under
/// `.cargo/bin`, means clearing the shim cache won't replace this binary.
fn cargo_install_hint() -> Option<String> {
    if std::env::var_os("DREAMD_BIN").is_some() {
        return Some("DREAMD_BIN is set".to_string());
    }
    let exe = std::env::current_exe().ok()?;
    let s = exe.to_string_lossy();
    if s.contains("/.cargo/bin/") || s.contains("\\.cargo\\bin\\") {
        return Some(format!("running from {s}"));
    }
    None
}

fn run_uninstall(args: UninstallArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let Some(daemon_home) = resolve_daemon_home() else {
        eprintln!("dreamd: error — {}", no_home_message());
        return ExitCode::from(2);
    };
    // Socket via the shared resolver ($DREAMD_SOCK else ~/.agent/dreamd.sock),
    // same as the status arm — never a hardcoded path.
    #[cfg(unix)]
    let socket = dreamd_core::client::resolve_daemon_socket();
    #[cfg(not(unix))]
    let socket: Option<PathBuf> = None;
    let cache_dir = resolve_shim_cache_dir();
    let home = resolve_home();
    let npx_dir = resolve_npx_dir();
    // lock-ok (AILAB-583): uninstall never opens a Tantivy index — it signals
    // this user's local servers and removes home/cache files.
    // See AGENTS.md no-hoisted-stdio-lock-across-tantivy.
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let req = commands::uninstall::UninstallRequest {
        cwd: &cwd,
        daemon_home: &daemon_home,
        socket: socket.as_deref(),
        cache_dir: cache_dir.as_deref(),
        npx_dir: npx_dir.as_deref(),
        keep_caches: args.keep_caches,
        all_npx: args.all_npx,
        quiet: args.quiet,
    };
    // AILAB-584 — the stop pass signals only processes attributable to this
    // invocation's home/cache. The scope is captured here rather than threaded
    // through `Stopper` so the command modules and their fake stoppers stay
    // unchanged.
    let scope = commands::lifecycle_cleanup::StopScope {
        home: home.as_deref(),
        cache_dir: cache_dir.as_deref(),
    };
    let mut stop =
        |e: &mut dyn std::io::Write| commands::lifecycle_cleanup::stop_local_servers(&scope, e);
    match commands::uninstall::run(&req, &mut stop, &mut out, &mut err) {
        Ok(()) => ExitCode::SUCCESS,
        Err(commands::uninstall::UninstallError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_update(args: UpdateArgs) -> ExitCode {
    // Socket via the shared resolver ($DREAMD_SOCK else ~/.agent/dreamd.sock).
    #[cfg(unix)]
    let socket = dreamd_core::client::resolve_daemon_socket();
    #[cfg(not(unix))]
    let socket: Option<PathBuf> = None;
    let cache_dir = resolve_shim_cache_dir();
    let home = resolve_home();
    // lock-ok (AILAB-583): update never opens a Tantivy index — it signals this
    // user's local servers and clears the native-binary cache.
    // See AGENTS.md no-hoisted-stdio-lock-across-tantivy.
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let req = commands::update::UpdateRequest {
        socket: socket.as_deref(),
        cache_dir: cache_dir.as_deref(),
        dry_run: args.dry_run,
        quiet: args.quiet,
        restart: args.restart,
        cargo_install_hint: cargo_install_hint(),
    };
    // AILAB-584 — see `run_uninstall`: scoped stop, captured not threaded.
    let scope = commands::lifecycle_cleanup::StopScope {
        home: home.as_deref(),
        cache_dir: cache_dir.as_deref(),
    };
    let mut stop =
        |e: &mut dyn std::io::Write| commands::lifecycle_cleanup::stop_local_servers(&scope, e);
    match commands::update::run(&req, &mut stop, &mut out, &mut err) {
        Ok(()) => ExitCode::SUCCESS,
        Err(commands::update::UpdateError::Io(e)) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_watch(args: WatchArgs) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    commands::watch::run(&cwd, &args)
}

/// `dreamd service install` (AILAB-190 / AILAB-169). Writes the per-user
/// service for this binary's `watch` against the project discovered from cwd:
/// a systemd --user unit on Linux (then `daemon-reload` + `enable --now`) or a
/// LaunchAgent plist on macOS (then `launchctl bootstrap gui/<uid>`; `--force`
/// overwrites an existing plist and is ignored on Linux). Console-only
/// one-shot: it only writes a service file and execs `systemctl` /
/// `launchctl`, never opens a Tantivy index, and uses unlocked
/// `println!`/`eprintln!` so no stdio lock is hoisted.
fn run_service_install(force: bool) -> ExitCode {
    let cwd = match current_dir_or_exit() {
        Ok(p) => p,
        Err(code) => return code,
    };
    let Some(home) = home_dir() else {
        eprintln!("dreamd: error — {}", no_home_message());
        return ExitCode::from(1);
    };
    service_exit(commands::service::run_install(&cwd, &home, force))
}

/// `dreamd service start` (AILAB-190 / AILAB-169): `systemctl --user start
/// dreamd.service` on Linux, `launchctl kickstart -k` on macOS. Same
/// console-only, index-free contract as `run_service_install`.
fn run_service_start() -> ExitCode {
    service_exit(commands::service::run_start())
}

/// `dreamd service restart` (AILAB-185): `systemctl --user restart
/// dreamd.service` on Linux, the same `launchctl kickstart -k` argv `start`
/// uses on macOS. Bounce-only — it never rewrites the unit / plist, so an
/// npx cache-path change still needs `dreamd service install` first. Not
/// `dreamd update --restart`, which stops local `mcp` / `watch` processes —
/// the supervised one included, since it runs under the same `HOME` — and
/// never brings the unit back up; this is the verb that does. Same
/// console-only, index-free contract as `run_service_start`.
fn run_service_restart() -> ExitCode {
    service_exit(commands::service::run_restart())
}

/// `dreamd service status` (AILAB-178): the OS supervisor's view of the unit /
/// LaunchAgent (running / stopped / failed / not-installed, PID, active-since)
/// plus the last `STATUS_LOG_TAIL_LINES` lines of `~/.agent/dreamd.log`, read
/// in-command like `run_status` does. Daemon-level UDS liveness is `dreamd
/// status` (WEG-103); the two are deliberately separate. Console-only,
/// index-free, unlocked `print!` — same contract as `run_service_install`.
fn run_service_status() -> ExitCode {
    let Some(home) = home_dir() else {
        eprintln!("dreamd: error — {}", no_home_message());
        return ExitCode::from(1);
    };
    let log_tail = commands::status::read_log_tail_n(
        &dreamd_core::layout::DaemonHome::new(home.join(".agent")).log_file(),
        commands::service::STATUS_LOG_TAIL_LINES,
    );
    service_exit(commands::service::run_status(&home, &log_tail))
}

/// `dreamd service uninstall` (AILAB-202): tear down the user-scope service
/// entry — the systemd --user unit or the macOS LaunchAgent — and print the
/// removed / preserved paths. Idempotent: a unit or plist that is already
/// gone is not an error. `--purge` additionally deletes the daemon home
/// `~/.agent/` after `--yes` or a typed `y` on a tty, and never a per-project
/// `.agent/` store.
///
/// Not the top-level `dreamd uninstall` (AILAB-226), which stops local
/// processes and clears caches: this verb never touches a process table.
/// No project root is needed, so `current_dir_or_exit` is deliberately not
/// called — one `HOME` read drives the unit path, the plist path and the
/// daemon home alike. Same console-only, index-free contract as
/// `run_service_install`: unlocked `println!`, no hoisted stdio lock.
fn run_service_uninstall(purge: bool, yes: bool) -> ExitCode {
    let Some(home) = home_dir() else {
        eprintln!("dreamd: error — {}", no_home_message());
        return ExitCode::from(1);
    };
    let daemon_home = home.join(".agent");
    service_exit(commands::service::run_uninstall(
        &home,
        &daemon_home,
        purge,
        yes,
    ))
}

/// Map a `service` verb result onto the exit-code contract. The usage
/// refusals — no supported backend on this host, no project root under cwd,
/// an existing LaunchAgent plist or scheduled-task XML without `--force`, and
/// the two `service uninstall --purge` confirmation refusals (non-tty stdin
/// without `--yes`, and a declined prompt) — exit 2 exactly like `dreamd
/// watch` without a project root; every other failure (service file write,
/// `systemctl` / `launchctl` / `schtasks` / `icacls` / `id -u` /
/// `whoami /user` spawn or non-zero exit) is a runtime error and exits 1. The
/// refusal copy lives in `ServiceError`'s `Display`.
fn service_exit(result: Result<(), commands::service::ServiceError>) -> ExitCode {
    use commands::service::ServiceError;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(
            e @ (ServiceError::NoSystemd
            | ServiceError::NoProjectRoot
            | ServiceError::PlistExists(_)
            | ServiceError::TaskExists(_)
            | ServiceError::NotATty
            | ServiceError::Declined),
        ) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

fn run_vectors_enable() -> ExitCode {
    // Stdout stays UNLOCKED: loading the model starts ONNX Runtime, which may
    // log through tracing to stderr. See AGENTS.md
    // no-hoisted-stdio-lock-across-tantivy.
    commands::vectors::run(&mut std::io::stdout(), &mut std::io::stderr())
}

fn run_version() -> ExitCode {
    // lock-ok (AILAB-583): version never opens a Tantivy index — it prints
    // compile-time build metadata only.
    // See AGENTS.md no-hoisted-stdio-lock-across-tantivy.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match commands::version::run(&mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dreamd: error — {e}");
            ExitCode::from(1)
        }
    }
}

/// Parse CLI args and dispatch to the matching subcommand handler.
///
/// Returns [`ExitCode`] directly so `main` stays a one-liner.
/// Exit `2` for usage errors; exit `1` for I/O / runtime errors.
pub fn run() -> ExitCode {
    // Same parse as `Cli::parse()`, split in two so the `ArgMatches` survives:
    // `setup`'s wizard needs `value_source` to tell a typed flag from a default
    // (AILAB-551 §1.7), which the derived struct alone cannot express.
    let matches = <Cli as clap::CommandFactory>::command().get_matches();
    // `from_arg_matches` (NOT `from_arg_matches_mut`) is load-bearing, not
    // stylistic: the `_mut` derive calls `matches.remove_subcommand()`, after
    // which `matches.subcommand_matches("setup")` below returns `None`, the
    // wizard silently degrades to `Interactive::default()` (ask nothing), and
    // `dreamd setup` on a TTY takes clap's defaults without a question. Pinned
    // by `cli::tests::setup_subcommand_matches_survive_from_arg_matches`.
    let cli = match <Cli as clap::FromArgMatches>::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };

    // WEG-32 / DR-004 — install the tracing subscriber once, before dispatch,
    // so every subcommand's `tracing` callsites land on stderr. AILAB-184: only
    // the daemon (`dreamd watch`) additionally gets the file layer at
    // ~/.agent/dreamd.log; see `wants_daemon_log` for why every other
    // invocation passes None and stays console-only. The log path is resolved
    // via the one `home_dir()` helper (empty HOME counts as none).
    // `_log_guard` MUST live until run() returns — `let _ =` would drop it
    // immediately and discard buffered file logs — and it is still
    // load-bearing whenever a file layer is installed.
    let log_file = if wants_daemon_log(cli.command.as_ref()) {
        home_dir().map(|h| dreamd_core::layout::DaemonHome::new(h.join(".agent")).log_file())
    } else {
        None
    };

    let _log_guard = dreamd_core::observability::init_tracing(log_file);

    if cli.version {
        println!("{VERSION_SHORT}");
        return ExitCode::SUCCESS;
    }

    let Some(command) = cli.command else {
        eprintln!("dreamd: error — no subcommand given. Try `dreamd --help`.");
        return ExitCode::from(2);
    };

    match command {
        Command::Archive(args) => run_archive(args),
        Command::Blame(args) => run_blame(args),
        Command::Doctor(args) => run_doctor(args),
        Command::Dream(args) => run_dream(args),
        Command::Forget(args) => run_forget(args),
        Command::Init(args) => run_init(args),
        Command::Mcp(args) => run_mcp(args),
        Command::Memory(args) => run_memory(args.command),
        Command::Migrate(args) => run_migrate(args),
        Command::Recall(args) => run_recall(args),
        Command::Score(args) => run_score(args),
        Command::SalienceDrift(args) => run_salience_drift(args),
        Command::Reset(args) => match args.command {
            ResetCommand::Workspace { yes } => run_reset_workspace(yes),
        },
        Command::Service(args) => match args.command {
            ServiceCommand::Install { force } => run_service_install(force),
            ServiceCommand::Start => run_service_start(),
            ServiceCommand::Restart => run_service_restart(),
            ServiceCommand::Status => run_service_status(),
            ServiceCommand::Uninstall { purge, yes } => run_service_uninstall(purge, yes),
        },
        Command::Setup(args) => {
            run_setup(args, setup_interactive(matches.subcommand_matches("setup")))
        }
        Command::Status => run_status(),
        Command::Uninstall(args) => run_uninstall(args),
        Command::Update(args) => run_update(args),
        Command::Watch(args) => run_watch(args),
        Command::Vectors(args) => match args.command {
            VectorsCommand::Enable => run_vectors_enable(),
        },
        Command::Version => run_version(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use dreamd_core::layout::resolve_home_from_vars;
    use std::ffi::OsString;

    #[test]
    fn auto_mode_rejected() {
        let cfg = Config {
            dream_cycle_mode: DreamCycleMode::Auto,
            ..Default::default()
        };
        assert!(
            check_dream_mode(&cfg).is_err(),
            "auto mode must be rejected by check_dream_mode"
        );
    }

    #[test]
    fn manual_mode_accepted() {
        let cfg = Config::default(); // default is Manual
        assert!(
            check_dream_mode(&cfg).is_ok(),
            "manual mode must be accepted by check_dream_mode"
        );
    }

    #[test]
    fn dream_args_validate_rejects_dry_and_auto() {
        let args = DreamArgs {
            dry: true,
            auto: true,
            no_commit: false,
            no_llm: false,
            share_personal: false,
        };
        let err = args.validate().unwrap_err();
        assert!(
            err.contains("mutually exclusive"),
            "expected mutual-exclusion message, got: {err}"
        );
    }

    #[test]
    fn dream_args_validate_accepts_each_flag_alone() {
        assert!(DreamArgs {
            dry: true,
            auto: false,
            no_commit: false,
            no_llm: false,
            share_personal: false,
        }
        .validate()
        .is_ok());
        assert!(DreamArgs {
            dry: false,
            auto: true,
            no_commit: false,
            no_llm: false,
            share_personal: false,
        }
        .validate()
        .is_ok());
        assert!(DreamArgs {
            dry: false,
            auto: false,
            no_commit: true,
            no_llm: false,
            share_personal: false,
        }
        .validate()
        .is_ok());
        assert!(DreamArgs {
            dry: false,
            auto: false,
            no_commit: false,
            no_llm: true,
            share_personal: false,
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn parses_version_flag() {
        let cli = Cli::try_parse_from(["dreamd", "--version"]).unwrap();
        assert!(cli.version);
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_dream_no_commit() {
        let cli = Cli::try_parse_from(["dreamd", "dream", "--no-commit"]).unwrap();
        match cli.command {
            Some(Command::Dream(args)) => {
                assert!(args.no_commit);
                assert!(!args.dry);
                assert!(!args.auto);
                assert!(!args.no_llm, "--no-llm defaults off");
                assert!(
                    !args.share_personal,
                    "--share-personal defaults off: the personal layer is excluded \
                     from the composition prompt unless it is passed"
                );
            }
            _ => panic!("expected Dream"),
        }
    }

    #[test]
    fn parses_dream_no_llm() {
        let cli = Cli::try_parse_from(["dreamd", "dream", "--no-llm"]).unwrap();
        match cli.command {
            Some(Command::Dream(args)) => {
                assert!(args.no_llm);
                assert!(!args.no_commit, "--no-llm must not imply --no-commit");
                assert!(!args.dry);
                assert!(!args.auto);
            }
            _ => panic!("expected Dream"),
        }
    }

    #[test]
    fn parses_dream_no_llm_with_no_commit() {
        // AILAB-204 §3.4 — the two flags are compatible and independent.
        let cli = Cli::try_parse_from(["dreamd", "dream", "--no-llm", "--no-commit"]).unwrap();
        match cli.command {
            Some(Command::Dream(args)) => {
                assert!(args.no_llm);
                assert!(args.no_commit);
                assert!(args.validate().is_ok(), "--no-llm + --no-commit is legal");
            }
            _ => panic!("expected Dream"),
        }
    }

    /// AILAB-199 §3.3 — the consent flag parses, is not hidden, and does not
    /// drag any other flag on with it.
    #[test]
    fn parses_dream_share_personal() {
        let cli = Cli::try_parse_from(["dreamd", "dream", "--share-personal"]).unwrap();
        match cli.command {
            Some(Command::Dream(args)) => {
                assert!(args.share_personal);
                assert!(!args.no_llm, "--share-personal must not imply --no-llm");
                assert!(!args.no_commit);
                assert!(!args.dry);
                assert!(!args.auto);
                assert!(args.validate().is_ok());
            }
            _ => panic!("expected Dream"),
        }
    }

    /// AILAB-199 §3.3 — compatible with both `--dry` and `--no-llm`. The
    /// combination is legal and discloses nothing: `--no-llm` never opens a
    /// request for the consent to apply to.
    #[test]
    fn parses_dream_share_personal_with_dry_and_no_llm() {
        let cli = Cli::try_parse_from(["dreamd", "dream", "--dry", "--no-llm", "--share-personal"])
            .unwrap();
        match cli.command {
            Some(Command::Dream(args)) => {
                assert!(args.share_personal);
                assert!(args.no_llm);
                assert!(args.dry);
                assert!(
                    args.validate().is_ok(),
                    "--dry + --no-llm + --share-personal is legal"
                );
            }
            _ => panic!("expected Dream"),
        }
    }

    #[test]
    fn parses_mcp_project_root_and_manual_only() {
        let cli = Cli::try_parse_from([
            "dreamd",
            "mcp",
            "--manual-only",
            "--project-root",
            "/tmp/proj",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Mcp(args)) => {
                assert!(args.manual_only);
                assert_eq!(
                    args.project_root.as_deref(),
                    Some(PathBuf::from("/tmp/proj").as_path())
                );
                assert!(args.bind.is_none(), "stdio stays the default");
                assert!(!args.insecure);
            }
            _ => panic!("expected Mcp"),
        }
    }

    #[test]
    fn parses_mcp_bind_and_insecure() {
        let cli =
            Cli::try_parse_from(["dreamd", "mcp", "--bind", "0.0.0.0:8080", "--insecure"]).unwrap();
        match cli.command {
            Some(Command::Mcp(args)) => {
                assert_eq!(args.bind.as_deref(), Some("0.0.0.0:8080"));
                assert!(args.insecure);
            }
            _ => panic!("expected Mcp"),
        }
    }

    #[test]
    fn parses_reset_workspace_yes() {
        let cli = Cli::try_parse_from(["dreamd", "reset", "workspace", "--yes"]).unwrap();
        match cli.command {
            Some(Command::Reset(ResetArgs {
                command: ResetCommand::Workspace { yes },
            })) => assert!(yes),
            _ => panic!("expected Reset workspace --yes"),
        }
    }

    #[test]
    fn parses_memory_subcommands() {
        let parse = |argv: &[&str]| match Cli::try_parse_from(argv).unwrap().command {
            Some(Command::Memory(MemoryArgs { command })) => command,
            _ => panic!("expected Memory for {argv:?}"),
        };
        assert!(matches!(
            parse(&["dreamd", "memory", "branch", "main"]),
            MemoryCommand::Branch { name } if name == "main"
        ));
        assert!(matches!(
            parse(&["dreamd", "memory", "checkout", "exp.1"]),
            MemoryCommand::Checkout { name } if name == "exp.1"
        ));
        assert!(matches!(
            parse(&["dreamd", "memory", "branches"]),
            MemoryCommand::Branches
        ));
        assert!(matches!(
            parse(&["dreamd", "memory", "delete", "old"]),
            MemoryCommand::Delete { name } if name == "old"
        ));
        assert!(Cli::try_parse_from(["dreamd", "memory", "branch"]).is_err());
        assert!(Cli::try_parse_from(["dreamd", "memory"]).is_err());
    }

    #[test]
    fn parses_memory_bisect() {
        let parse = |argv: &[&str]| match Cli::try_parse_from(argv).unwrap().command {
            Some(Command::Memory(MemoryArgs {
                command: MemoryCommand::Bisect(BisectArgs { command }),
            })) => command,
            _ => panic!("expected Memory Bisect for {argv:?}"),
        };
        assert!(matches!(
            parse(&["dreamd", "memory", "bisect", "start", "--good", "a", "--bad", "b"]),
            BisectCommand::Start { good, bad, auto_test: None } if good == "a" && bad == "b"
        ));
        assert!(matches!(
            parse(&[
                "dreamd", "memory", "bisect", "start", "--good", "a", "--bad", "b",
                "--auto-test", "make check",
            ]),
            BisectCommand::Start { auto_test: Some(s), .. } if s == "make check"
        ));
        assert!(matches!(
            parse(&["dreamd", "memory", "bisect", "good"]),
            BisectCommand::Good
        ));
        assert!(matches!(
            parse(&["dreamd", "memory", "bisect", "bad"]),
            BisectCommand::Bad
        ));
        assert!(matches!(
            parse(&["dreamd", "memory", "bisect", "run", "./t.sh --x"]),
            BisectCommand::Run { script } if script == "./t.sh --x"
        ));
        assert!(
            Cli::try_parse_from(["dreamd", "memory", "bisect", "start", "--good", "a"]).is_err()
        );
        assert!(Cli::try_parse_from(["dreamd", "memory", "bisect", "run"]).is_err());
        assert!(Cli::try_parse_from(["dreamd", "memory", "bisect"]).is_err());
    }

    #[test]
    fn parses_memory_diff() {
        let parse = |argv: &[&str]| match Cli::try_parse_from(argv).unwrap().command {
            Some(Command::Memory(MemoryArgs { command })) => command,
            _ => panic!("expected Memory for {argv:?}"),
        };
        assert!(matches!(
            parse(&["dreamd", "memory", "diff", "a", "b"]),
            MemoryCommand::Diff { from, to, unified: false, json: false }
                if from == "a" && to == "b"
        ));
        assert!(matches!(
            parse(&["dreamd", "memory", "diff", "a", "b", "--unified"]),
            MemoryCommand::Diff {
                unified: true,
                json: false,
                ..
            }
        ));
        assert!(matches!(
            parse(&["dreamd", "memory", "diff", "--json", "a", "b"]),
            MemoryCommand::Diff {
                unified: false,
                json: true,
                ..
            }
        ));
        assert!(Cli::try_parse_from(["dreamd", "memory", "diff", "a"]).is_err());
    }

    #[test]
    fn parses_service_install_and_start() {
        let cli = Cli::try_parse_from(["dreamd", "service", "install"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Service(ServiceArgs {
                command: ServiceCommand::Install { force: false }
            }))
        ));
        let cli = Cli::try_parse_from(["dreamd", "service", "install", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Service(ServiceArgs {
                command: ServiceCommand::Install { force: true }
            }))
        ));
        let cli = Cli::try_parse_from(["dreamd", "service", "start"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Service(ServiceArgs {
                command: ServiceCommand::Start
            }))
        ));
        assert!(
            Cli::try_parse_from(["dreamd", "service"]).is_err(),
            "bare `service` needs a verb"
        );
    }

    #[test]
    fn parses_service_status() {
        let cli = Cli::try_parse_from(["dreamd", "service", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Service(ServiceArgs {
                command: ServiceCommand::Status
            }))
        ));
    }

    /// AILAB-185 — the fourth nested verb. Takes no flags: it bounces the unit
    /// `service install` already wrote and never rewrites it.
    #[test]
    fn parses_service_restart() {
        let cli = Cli::try_parse_from(["dreamd", "service", "restart"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Service(ServiceArgs {
                command: ServiceCommand::Restart
            }))
        ));
    }

    /// AILAB-202 — the nested verb, and both of its flags. `--yes` alone
    /// parses too: it is the `--purge` confirmation, so on its own it is
    /// accepted and ignored, the same shape as `--force` on a Linux install.
    #[test]
    fn parses_service_uninstall() {
        for (argv, want_purge, want_yes) in [
            (vec!["dreamd", "service", "uninstall"], false, false),
            (
                vec!["dreamd", "service", "uninstall", "--purge"],
                true,
                false,
            ),
            (
                vec!["dreamd", "service", "uninstall", "--purge", "--yes"],
                true,
                true,
            ),
            (vec!["dreamd", "service", "uninstall", "--yes"], false, true),
        ] {
            let cli = Cli::try_parse_from(&argv).unwrap();
            match cli.command {
                Some(Command::Service(ServiceArgs {
                    command: ServiceCommand::Uninstall { purge, yes },
                })) => {
                    assert_eq!(purge, want_purge, "{argv:?}");
                    assert_eq!(yes, want_yes, "{argv:?}");
                }
                _ => panic!("expected Service(Uninstall) for {argv:?}"),
            }
        }
    }

    #[test]
    fn service_exit_maps_usage_refusals_to_2_and_runtime_failures_to_1() {
        use commands::service::ServiceError;
        assert_eq!(service_exit(Ok(())), ExitCode::SUCCESS);
        assert_eq!(
            service_exit(Err(ServiceError::NoSystemd)),
            ExitCode::from(2),
            "no systemd --user is a usage refusal, like watch without a project"
        );
        assert_eq!(
            service_exit(Err(ServiceError::NoProjectRoot)),
            ExitCode::from(2),
            "no project root is a usage refusal, like watch without a project"
        );
        assert_eq!(
            service_exit(Err(ServiceError::PlistExists(PathBuf::from(
                "/h/Library/LaunchAgents/dev.dreamd.dreamd.plist"
            )))),
            ExitCode::from(2),
            "an existing plist without --force is a usage refusal"
        );
        assert_eq!(
            service_exit(Err(ServiceError::TaskExists(PathBuf::from(
                "/h/AppData/Roaming/dreamd/dev.dreamd.dreamd.xml"
            )))),
            ExitCode::from(2),
            "an existing scheduled-task XML without --force is a usage refusal, like the plist"
        );
        assert_eq!(
            service_exit(Err(ServiceError::NotATty)),
            ExitCode::from(2),
            "non-tty stdin without --yes is a usage refusal, like reset workspace"
        );
        assert_eq!(
            service_exit(Err(ServiceError::Declined)),
            ExitCode::from(2),
            "a declined --purge prompt is a usage refusal, like reset workspace"
        );
        assert_eq!(
            service_exit(Err(ServiceError::Stdin(std::io::Error::other("x")))),
            ExitCode::from(1),
            "an unreadable confirmation is a runtime error"
        );
        assert_eq!(
            service_exit(Err(ServiceError::Spawn {
                program: "systemctl",
                source: std::io::Error::other("x"),
            })),
            ExitCode::from(1),
            "a systemctl spawn failure is a runtime error"
        );
        assert_eq!(
            service_exit(Err(ServiceError::Uid("garbage".into()))),
            ExitCode::from(1),
            "an unreadable uid is a runtime error"
        );
        assert_eq!(
            service_exit(Err(ServiceError::CurrentExe(std::io::Error::other("x")))),
            ExitCode::from(1),
            "an unresolvable binary path is a runtime error"
        );
    }

    /// AILAB-203 — `$HOME` wins wherever it is set, on **both** sides of the
    /// `windows` switch: Git-Bash and WSL do export it, and a user who went to
    /// the trouble of setting it must not be overruled by the profile
    /// directory. The switch is an argument rather than a `#[cfg(windows)]`
    /// branch precisely so both answers are ordinary tests on any host — a
    /// Windows-only path nobody can run is how a Windows-only bug ships.
    #[test]
    fn resolve_home_prefers_home_over_userprofile_on_both_targets() {
        for windows in [true, false] {
            assert_eq!(
                resolve_home_from_vars(
                    Some(OsString::from("/home/u")),
                    Some(OsString::from("C:\\Users\\u")),
                    windows,
                ),
                Some(PathBuf::from("/home/u")),
                "HOME must win (windows={windows})"
            );
        }
    }

    /// The fallback itself, and the fact that it is Windows-only. Native
    /// `cmd.exe` and PowerShell set `USERPROFILE` and no `HOME` at all, so
    /// without this `dreamd service install` would never find `~/.agent` on
    /// the one OS whose service path depends on it; Unix never consults the
    /// variable, so the identical environment answers `None` there.
    #[test]
    fn resolve_home_falls_back_to_userprofile_only_on_windows() {
        assert_eq!(
            resolve_home_from_vars(None, Some(OsString::from("C:\\Users\\u")), true),
            Some(PathBuf::from("C:\\Users\\u"))
        );
        assert_eq!(
            resolve_home_from_vars(None, Some(OsString::from("C:\\Users\\u")), false),
            None,
            "USERPROFILE is never read on Unix"
        );
    }

    /// A variable that is set but *empty* is an ordinary environment (`env -i`,
    /// a systemd `Environment=HOME=`, a Docker `ENV HOME=`, several CI
    /// runners), and every path derived from it silently becomes **relative**.
    /// Empty is therefore treated exactly like unset — for both variables,
    /// since a blank `%USERPROFILE%` derails a path in precisely the same way.
    #[test]
    fn resolve_home_treats_empty_variables_as_unset() {
        for windows in [true, false] {
            assert_eq!(
                resolve_home_from_vars(Some(OsString::new()), None, windows),
                None,
                "an empty HOME must not become a relative root (windows={windows})"
            );
            assert_eq!(
                resolve_home_from_vars(
                    Some(OsString::new()),
                    Some(OsString::from("C:\\Users\\u")),
                    windows,
                ),
                if windows {
                    Some(PathBuf::from("C:\\Users\\u"))
                } else {
                    None
                },
                "an empty HOME must behave exactly like an unset one (windows={windows})"
            );
            assert_eq!(
                resolve_home_from_vars(None, Some(OsString::new()), windows),
                None,
                "an empty USERPROFILE is unset too (windows={windows})"
            );
            assert_eq!(
                resolve_home_from_vars(Some(OsString::new()), Some(OsString::new()), windows),
                None,
                "two empty variables are still no home (windows={windows})"
            );
        }
    }

    /// Neither variable set is the genuine "no home directory" case, which
    /// every caller already has a defined behaviour for (skip with a note, or
    /// refuse to attribute anything).
    #[test]
    fn resolve_home_without_either_variable_is_none() {
        assert_eq!(resolve_home_from_vars(None, None, true), None);
        assert_eq!(resolve_home_from_vars(None, None, false), None);
    }

    #[test]
    fn parses_forget_id_dry_run_and_proof() {
        let id = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV";
        match Cli::try_parse_from(["dreamd", "forget", id])
            .unwrap()
            .command
        {
            Some(Command::Forget(args)) => {
                assert_eq!(args.id, id);
                assert!(!args.dry_run);
                assert!(args.proof.is_none());
            }
            _ => panic!("expected Forget"),
        }
        match Cli::try_parse_from(["dreamd", "forget", id, "--dry-run"])
            .unwrap()
            .command
        {
            Some(Command::Forget(args)) => assert!(args.dry_run),
            _ => panic!("expected Forget --dry-run"),
        }
        match Cli::try_parse_from(["dreamd", "forget", id, "--proof", "out.json"])
            .unwrap()
            .command
        {
            Some(Command::Forget(args)) => {
                assert_eq!(
                    args.proof.as_deref(),
                    Some(std::path::Path::new("out.json"))
                );
            }
            _ => panic!("expected Forget --proof"),
        }
    }

    #[test]
    fn parses_forget_rejects_dry_run_with_proof() {
        assert!(Cli::try_parse_from([
            "dreamd",
            "forget",
            "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--dry-run",
            "--proof",
            "out.json",
        ])
        .is_err());
    }

    #[test]
    fn parses_archive_force_unpin_with_id() {
        let cli = Cli::try_parse_from(["dreamd", "archive", "--force-unpin", "evt_123"]).unwrap();
        match cli.command {
            Some(Command::Archive(args)) => {
                assert!(args.force_unpin);
                assert_eq!(args.id.as_deref(), Some("evt_123"));
                assert!(!args.all);
            }
            _ => panic!("expected Archive --force-unpin evt_123"),
        }
    }

    #[test]
    fn parses_archive_force_unpin_all() {
        let cli = Cli::try_parse_from(["dreamd", "archive", "--force-unpin", "--all"]).unwrap();
        match cli.command {
            Some(Command::Archive(args)) => {
                assert!(args.force_unpin);
                assert!(args.id.is_none());
                assert!(args.all);
            }
            _ => panic!("expected Archive --force-unpin --all"),
        }
    }

    #[test]
    fn parses_migrate_from_and_to() {
        let cli =
            Cli::try_parse_from(["dreamd", "migrate", "--from", "1.0.0", "--to", "1.0.0"]).unwrap();
        match cli.command {
            Some(Command::Migrate(args)) => {
                assert_eq!(args.from, "1.0.0");
                assert_eq!(args.to, "1.0.0");
            }
            _ => panic!("expected Migrate"),
        }
    }

    #[test]
    fn parses_blame_defaults_k_to_five() {
        let cli = Cli::try_parse_from(["dreamd", "blame", "axum error"]).unwrap();
        match cli.command {
            Some(Command::Blame(args)) => {
                assert_eq!(args.query, "axum error");
                assert_eq!(args.k, 5, "blame default -k is 5 (recall stays 10)");
                assert!(!args.explain);
                assert!(!args.json);
            }
            _ => panic!("expected Blame"),
        }
    }

    #[test]
    fn parses_blame_k_explain_and_json() {
        let cli =
            Cli::try_parse_from(["dreamd", "blame", "tokio", "-k", "3", "--explain", "--json"])
                .unwrap();
        match cli.command {
            Some(Command::Blame(args)) => {
                assert_eq!(args.query, "tokio");
                assert_eq!(args.k, 3);
                assert!(args.explain);
                assert!(args.json);
            }
            _ => panic!("expected Blame"),
        }
    }

    #[test]
    fn parses_recall_defaults_k_to_ten() {
        let cli = Cli::try_parse_from(["dreamd", "recall", "axum error"]).unwrap();
        match cli.command {
            Some(Command::Recall(args)) => {
                assert_eq!(args.query, "axum error");
                assert_eq!(args.k, 10, "CLI default -k is 10 (HTTP/MCP stay at 5)");
                assert!(!args.explain);
            }
            _ => panic!("expected Recall"),
        }
    }

    #[test]
    fn parses_recall_without_and_without_cluster() {
        let cli = Cli::try_parse_from([
            "dreamd",
            "recall",
            "q",
            "--without",
            "evt_a",
            "--without",
            "evt_b",
            "--without-cluster",
            "rust::error_handling",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Recall(args)) => {
                assert_eq!(args.query, "q");
                assert_eq!(args.without, vec!["evt_a", "evt_b"]);
                assert_eq!(args.without_cluster, vec!["rust::error_handling"]);
                assert_eq!(args.k, 10, "--without does not move the CLI default -k");
            }
            _ => panic!("expected Recall"),
        }
    }

    #[test]
    fn parses_recall_k_and_explain() {
        let cli =
            Cli::try_parse_from(["dreamd", "recall", "tokio", "-k", "3", "--explain"]).unwrap();
        match cli.command {
            Some(Command::Recall(args)) => {
                assert_eq!(args.query, "tokio");
                assert_eq!(args.k, 3);
                assert!(args.explain);
            }
            _ => panic!("expected Recall"),
        }
    }

    #[test]
    fn parses_score_defaults_n_to_hundred() {
        let cli = Cli::try_parse_from(["dreamd", "score"]).unwrap();
        match cli.command {
            Some(Command::Score(args)) => {
                assert_eq!(args.n, 100, "CLI default -n is 100");
                assert!(!args.explain);
            }
            _ => panic!("expected Score"),
        }
    }

    #[test]
    fn parses_score_n_and_explain() {
        let cli = Cli::try_parse_from(["dreamd", "score", "-n", "3", "--explain"]).unwrap();
        match cli.command {
            Some(Command::Score(args)) => {
                assert_eq!(args.n, 3);
                assert!(args.explain);
            }
            _ => panic!("expected Score"),
        }
    }

    #[test]
    fn parses_salience_drift_defaults_to_text() {
        let cli = Cli::try_parse_from(["dreamd", "salience-drift"]).unwrap();
        match cli.command {
            Some(Command::SalienceDrift(args)) => assert!(!args.json),
            _ => panic!("expected SalienceDrift"),
        }
    }

    #[test]
    fn parses_vectors_enable() {
        let cli = Cli::try_parse_from(["dreamd", "vectors", "enable"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Vectors(VectorsArgs {
                command: VectorsCommand::Enable
            }))
        ));
    }

    #[test]
    fn parses_salience_drift_json() {
        let cli = Cli::try_parse_from(["dreamd", "salience-drift", "--json"]).unwrap();
        match cli.command {
            Some(Command::SalienceDrift(args)) => assert!(args.json),
            _ => panic!("expected SalienceDrift"),
        }
    }

    #[test]
    fn parses_watch_and_status_and_doctor() {
        for (argv, expect) in [
            (vec!["dreamd", "watch"], "watch"),
            (vec!["dreamd", "status"], "status"),
            (vec!["dreamd", "doctor"], "doctor"),
            (vec!["dreamd", "version"], "version"),
        ] {
            let cli = Cli::try_parse_from(&argv).unwrap();
            let label = match cli.command {
                Some(Command::Watch(_)) => "watch",
                Some(Command::Status) => "status",
                Some(Command::Doctor(_)) => "doctor",
                Some(Command::Version) => "version",
                _ => panic!("unexpected command for {argv:?}"),
            };
            assert_eq!(label, expect);
        }
    }

    /// AILAB-184 — only the daemon owns `~/.agent/dreamd.log`. Everything else,
    /// including the long-running `mcp` server (several run concurrently under
    /// an IDE), stays console-only so nothing truncates the daemon's file.
    #[test]
    fn wants_daemon_log_true_for_watch_only() {
        let watch = Cli::try_parse_from(["dreamd", "watch"]).unwrap();
        assert!(
            wants_daemon_log(watch.command.as_ref()),
            "watch is the daemon and must get the file layer"
        );

        for argv in [
            vec!["dreamd", "status"],
            vec!["dreamd", "mcp"],
            vec!["dreamd", "init"],
            vec!["dreamd", "doctor"],
            vec!["dreamd", "version"],
            vec!["dreamd", "service", "install"],
            vec!["dreamd", "service", "install", "--force"],
            vec!["dreamd", "service", "start"],
            vec!["dreamd", "service", "restart"],
            vec!["dreamd", "service", "status"],
            vec!["dreamd", "service", "uninstall"],
            vec!["dreamd", "service", "uninstall", "--purge"],
            vec!["dreamd", "service", "uninstall", "--purge", "--yes"],
        ] {
            let cli = Cli::try_parse_from(&argv).unwrap();
            assert!(
                !wants_daemon_log(cli.command.as_ref()),
                "{argv:?} must stay console-only"
            );
        }
    }

    /// Bare `dreamd --version` leaves `command == None` and must not open the
    /// daemon log either (AILAB-184 consequence #2).
    #[test]
    fn wants_daemon_log_false_without_subcommand() {
        assert!(!wants_daemon_log(None));

        let bare = Cli::try_parse_from(["dreamd", "--version"]).unwrap();
        assert!(bare.command.is_none());
        assert!(!wants_daemon_log(bare.command.as_ref()));
    }

    #[test]
    fn parses_doctor_repair_and_cluster_health() {
        let bare = Cli::try_parse_from(["dreamd", "doctor"]).unwrap();
        match bare.command {
            Some(Command::Doctor(args)) => {
                assert!(!args.repair);
                assert!(!args.cluster_health);
                assert!(!args.provenance);
            }
            _ => panic!("expected Doctor"),
        }
        let both =
            Cli::try_parse_from(["dreamd", "doctor", "--repair", "--cluster-health"]).unwrap();
        match both.command {
            Some(Command::Doctor(args)) => {
                assert!(args.repair);
                assert!(args.cluster_health);
            }
            _ => panic!("expected Doctor with both flags"),
        }
        let provenance = Cli::try_parse_from(["dreamd", "doctor", "--provenance"]).unwrap();
        match provenance.command {
            Some(Command::Doctor(args)) => {
                assert!(args.provenance);
                assert!(!args.repair);
                assert!(!args.cluster_health);
            }
            _ => panic!("expected Doctor --provenance"),
        }
    }

    #[test]
    fn parses_init_quiet_and_uninstall() {
        let quiet = Cli::try_parse_from(["dreamd", "init", "--quiet"]).unwrap();
        match quiet.command {
            Some(Command::Init(args)) => {
                assert!(args.quiet);
                assert!(!args.uninstall_project);
            }
            _ => panic!("expected Init --quiet"),
        }
        let uninstall = Cli::try_parse_from(["dreamd", "init", "--uninstall-project"]).unwrap();
        match uninstall.command {
            Some(Command::Init(args)) => assert!(args.uninstall_project),
            _ => panic!("expected Init --uninstall-project"),
        }
    }

    #[test]
    fn parses_setup_flags() {
        let cli = Cli::try_parse_from([
            "dreamd",
            "setup",
            "--yes",
            "--harness",
            "none",
            "--no-write-mcp",
            "--no-start-watch",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Setup(args)) => {
                assert!(args.yes);
                assert_eq!(args.harness, commands::setup::Harness::None);
                assert!(args.no_write_mcp);
                assert!(!args.wants_start_watch());
                assert!(args.dry_run);
            }
            _ => panic!("expected Setup"),
        }
    }

    #[test]
    fn setup_defaults_write_mcp_for_both_harnesses() {
        let cli = Cli::try_parse_from(["dreamd", "setup"]).unwrap();
        match cli.command {
            Some(Command::Setup(args)) => {
                assert!(!args.yes);
                assert_eq!(
                    args.harness,
                    commands::setup::Harness::Both,
                    "default harness is both (design: MCP config written by default)"
                );
                assert!(!args.no_write_mcp);
                assert!(!args.force, "conflicts refuse unless --force is given");
                assert!(!args.wants_start_watch(), "watch is print-only by default");
                assert!(!args.dry_run);
            }
            _ => panic!("expected Setup"),
        }
    }

    #[test]
    fn setup_parses_force() {
        let cli = Cli::try_parse_from(["dreamd", "setup", "--force"]).unwrap();
        match cli.command {
            Some(Command::Setup(args)) => assert!(args.force),
            _ => panic!("expected Setup"),
        }
    }

    #[test]
    fn setup_start_watch_pair_is_last_one_wins() {
        let on = Cli::try_parse_from(["dreamd", "setup", "--start-watch"]).unwrap();
        match on.command {
            Some(Command::Setup(args)) => assert!(args.wants_start_watch()),
            _ => panic!("expected Setup"),
        }
        // Both flags must parse (no conflict error); the last one wins.
        let off =
            Cli::try_parse_from(["dreamd", "setup", "--start-watch", "--no-start-watch"]).unwrap();
        match off.command {
            Some(Command::Setup(args)) => assert!(!args.wants_start_watch()),
            _ => panic!("expected Setup"),
        }
    }

    /// Resolve the wizard's prompt plan the way `run()` does: real parse, then
    /// `value_source` off the subcommand's own matches.
    fn interactive_for(argv: &[&str]) -> commands::setup::Interactive {
        let matches = <Cli as clap::CommandFactory>::command()
            .try_get_matches_from(argv)
            .unwrap();
        setup_interactive(matches.subcommand_matches("setup"))
    }

    /// The seam the whole wizard hangs off. `run()` parses once and then reads
    /// `value_source` back off `matches.subcommand_matches("setup")`; switching
    /// that parse to `from_arg_matches_mut` would consume the subcommand
    /// (`remove_subcommand`), leave `subcommand_matches("setup")` as `None`, and
    /// delete every prompt — with no other test able to notice, because they all
    /// build their own `ArgMatches`. This one asserts the real sequence.
    #[test]
    fn setup_subcommand_matches_survive_from_arg_matches() {
        let matches = <Cli as clap::CommandFactory>::command()
            .try_get_matches_from(["dreamd", "setup"])
            .unwrap();
        let cli = <Cli as clap::FromArgMatches>::from_arg_matches(&matches)
            .expect("bare `dreamd setup` must parse");
        assert!(matches!(cli.command, Some(Command::Setup(_))));
        let sub = matches.subcommand_matches("setup");
        assert!(
            sub.is_some(),
            "`from_arg_matches` must leave the setup subcommand on the matches; \
             the `_mut` variant removes it and silently disables the wizard"
        );
        // And the plan derived from it is the real one, not the empty default.
        let plan = setup_interactive(sub);
        assert!(
            plan.ask_harness && plan.ask_watch,
            "a surviving subcommand must yield the live prompt plan, not \
             `Interactive::default()`"
        );

        // The hazard is real, not hypothetical: the `_mut` variant consumes the
        // subcommand, and the wizard collapses to asking nothing.
        let mut consumed = matches;
        let _ = <Cli as clap::FromArgMatches>::from_arg_matches_mut(&mut consumed)
            .expect("bare `dreamd setup` must parse");
        assert!(
            consumed.subcommand_matches("setup").is_none(),
            "if this ever stops holding, the comment at the `run()` callsite \
             needs revisiting"
        );
        let degraded = setup_interactive(consumed.subcommand_matches("setup"));
        assert!(
            !degraded.ask_harness && !degraded.ask_watch,
            "the `_mut` path is exactly the silent no-prompt regression this \
             test exists to catch"
        );
    }

    #[test]
    fn bare_setup_asks_both_questions() {
        let plan = interactive_for(&["dreamd", "setup"]);
        assert!(
            plan.ask_harness,
            "no --harness typed, so the harness prompt runs \
             (the clap default `both` must not read as an answer)"
        );
        assert!(
            plan.ask_watch,
            "no watch flag typed, so the watch prompt runs"
        );
    }

    #[test]
    fn typed_flags_skip_their_prompts() {
        // AILAB-551 §1.7: a flag the user typed wins over its prompt. Note
        // `--harness both` IS the clap default — detection must be by source.
        let plan = interactive_for(&["dreamd", "setup", "--harness", "both"]);
        assert!(
            !plan.ask_harness,
            "--harness both was typed; do not ask again"
        );
        assert!(plan.ask_watch, "the watch question is still unanswered");

        for watch_flag in ["--start-watch", "--no-start-watch"] {
            let plan = interactive_for(&["dreamd", "setup", watch_flag]);
            assert!(
                !plan.ask_watch,
                "{watch_flag} was typed; do not ask the watch question"
            );
            assert!(plan.ask_harness, "the harness question is still unanswered");
        }

        // Both watch flags: last-one-wins in clap, still an explicit answer.
        let plan = interactive_for(&["dreamd", "setup", "--start-watch", "--no-start-watch"]);
        assert!(!plan.ask_watch, "an overridden pair is still user-typed");
    }

    #[test]
    fn parses_uninstall_flags() {
        let cli = Cli::try_parse_from(["dreamd", "uninstall", "--keep-caches", "--all-npx", "-q"])
            .unwrap();
        match cli.command {
            Some(Command::Uninstall(args)) => {
                assert!(args.keep_caches);
                assert!(args.all_npx);
                assert!(args.quiet);
            }
            _ => panic!("expected Uninstall"),
        }
        let defaults = Cli::try_parse_from(["dreamd", "uninstall"]).unwrap();
        match defaults.command {
            Some(Command::Uninstall(args)) => {
                assert!(!args.keep_caches);
                assert!(!args.all_npx);
                assert!(!args.quiet);
            }
            _ => panic!("expected Uninstall with default flags"),
        }
    }

    #[test]
    fn parses_update_flags() {
        let cli =
            Cli::try_parse_from(["dreamd", "update", "--dry-run", "-q", "--restart"]).unwrap();
        match cli.command {
            Some(Command::Update(args)) => {
                assert!(args.dry_run);
                assert!(args.quiet);
                assert!(args.restart);
            }
            _ => panic!("expected Update"),
        }
        let defaults = Cli::try_parse_from(["dreamd", "update"]).unwrap();
        match defaults.command {
            Some(Command::Update(args)) => {
                assert!(!args.dry_run);
                assert!(!args.quiet);
                assert!(!args.restart);
            }
            _ => panic!("expected Update with default flags"),
        }
    }

    #[test]
    fn render_man_page_includes_binary_name_and_subcommands() {
        let page = String::from_utf8(render_man_page().unwrap()).unwrap();
        assert!(page.contains("dreamd"), "man page must name the binary");
        for sub in [
            "init",
            "dream",
            "mcp",
            "watch",
            "doctor",
            "status",
            "version",
            "reset",
            "archive",
            "migrate",
            "uninstall",
            "update",
            "setup",
            "service",
            "vectors",
        ] {
            assert!(
                page.contains(sub),
                "man page must document subcommand {sub}; page length={}",
                page.len()
            );
        }
    }
}
