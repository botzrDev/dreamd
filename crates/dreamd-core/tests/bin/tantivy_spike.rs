//! WEG-24 measurement spike: tantivy IndexWriter open-cost, multi-process
//! reader reload, and lock-cleanup-under-SIGKILL.
//!
//! NOT a shipping binary. Tantivy is a dev-dep only; this binary is excluded
//! from `test`, `bench`, and `doc` in `Cargo.toml`. Its single purpose is to
//! produce numbers for `docs/spikes/tantivy-indexwriter.md` so we can decide
//! GO / ESCALATE on WEG-41.
//!
//! Modes (selected by argv[1]):
//!
//! - (no arg) — run full suite, print report-shaped output on stdout, exit 0.
//! - `cold-open <index-path>` — open IndexWriter once, print elapsed ms on
//!   stdout, exit 0. Re-execed from the parent for cold-open samples.
//! - `child-writer-stall <index-path>` — acquire writer lock, hold forever
//!   (parent SIGKILLs us).
//! - `child-reader-reload <index-path>` — open reader, reload, print elapsed
//!   ms and doc count, exit 0.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use dreamd_protocol::{AgentLearning, EventId};
use tantivy::collector::Count;
use tantivy::doc;
use tantivy::query::AllQuery;
use tantivy::schema::{Schema, FAST, STORED, TEXT};
use tantivy::{Index, IndexWriter, ReloadPolicy, TantivyDocument};

const WRITER_HEAP_BYTES: usize = 50_000_000;
const PLACEHOLDER_EVENT_ID: &str = "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV";

// ---------------------------------------------------------------------------
// Schema (matches CLAUDE.md "Load-bearing engineering decisions" §2)
// ---------------------------------------------------------------------------

struct SchemaBundle {
    schema: Schema,
    content: tantivy::schema::Field,
    timestamp_sec: tantivy::schema::Field,
    pain: tantivy::schema::Field,
    importance: tantivy::schema::Field,
    recurrence: tantivy::schema::Field,
}

fn build_schema() -> SchemaBundle {
    let mut sb = Schema::builder();
    let content = sb.add_text_field("content", TEXT | STORED);
    let timestamp_sec = sb.add_u64_field("timestamp_sec", FAST);
    let pain = sb.add_f64_field("pain", FAST);
    let importance = sb.add_f64_field("importance", FAST);
    let recurrence = sb.add_u64_field("recurrence", FAST);
    SchemaBundle {
        schema: sb.build(),
        content,
        timestamp_sec,
        pain,
        importance,
        recurrence,
    }
}

// ---------------------------------------------------------------------------
// Synthetic AgentLearning generation
// ---------------------------------------------------------------------------

/// Tiny deterministic LCG so corpora are reproducible without pulling `rand`.
struct Lcg(u64);
impl Lcg {
    fn next_u64(&mut self) -> u64 {
        // Numerical Recipes constants.
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn next_f32(&mut self, lo: f32, hi: f32) -> f32 {
        let r = (self.next_u64() >> 11) as f32 / (1u64 << 53) as f32;
        lo + r * (hi - lo)
    }
}

const SKILL_ACTIONS: &[&str] = &[
    "rust.cargo.test",
    "rust.cargo.build",
    "rust.borrow_checker",
    "python.pytest.fixture",
    "ts.vite.hmr",
    "git.rebase.interactive",
    "shell.find.exec",
    "docker.compose.healthcheck",
    "k8s.kubectl.apply",
    "sql.postgres.explain",
];

const HARNESSES: &[&str] = &["claude-code", "cursor", "opencode", "windsurf"];

const SNIPPETS: &[&str] = &[
    "borrow checker complained about lifetime 'a outliving the closure",
    "pytest fixture scope=session caused state to bleed across files",
    "rebase --continue dropped commits silently when conflict markers stayed",
    "vite HMR did not pick up files outside the project root",
    "kubectl apply -f failed because the namespace label was missing",
    "explain analyze showed a sequential scan instead of the partial index",
    "find with -exec needs the trailing \\; or + or it errors",
    "compose healthcheck retries multiplied container start latency",
    "cargo test on a single binary crate cannot see pub(crate) symbols",
    "tantivy index sort was removed in 0.23, score at query time instead",
];

fn synth_learning(i: u64, rng: &mut Lcg) -> AgentLearning {
    let skill = SKILL_ACTIONS[(i as usize) % SKILL_ACTIONS.len()];
    let harness = HARNESSES[(i as usize) % HARNESSES.len()];
    let snippet = SNIPPETS[(i as usize) % SNIPPETS.len()];
    // Spread timestamps over the last ~60 days.
    let base_sec = Utc
        .with_ymd_and_hms(2026, 3, 14, 0, 0, 0)
        .unwrap()
        .timestamp();
    let offset_sec = (rng.next_u64() % (60 * 24 * 60 * 60)) as i64;
    let ts = Utc.timestamp_opt(base_sec + offset_sec, 0).unwrap();
    AgentLearning {
        schema_version: "1.0".to_string(),
        id: EventId::parse(PLACEHOLDER_EVENT_ID).expect("placeholder parses"),
        timestamp: ts,
        pain: rng.next_f32(0.0, 10.0),
        importance: rng.next_f32(0.0, 10.0),
        pinned: false,
        skill_action: skill.to_string(),
        source_harness: harness.to_string(),
        content: format!("[{i}] {snippet} — skill={skill} harness={harness}"),
    }
}

// ---------------------------------------------------------------------------
// Corpus build
// ---------------------------------------------------------------------------

fn build_corpus(path: &Path, n: u64) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(path)?;
    let bundle = build_schema();
    let index = Index::create_in_dir(path, bundle.schema.clone())?;
    let mut writer: IndexWriter = index.writer(WRITER_HEAP_BYTES)?;
    let mut rng = Lcg(0xD15EA5E_u64 ^ n);
    for i in 0..n {
        let learning = synth_learning(i, &mut rng);
        writer.add_document(doc!(
            bundle.content => learning.content.clone(),
            bundle.timestamp_sec => learning.timestamp.timestamp() as u64,
            bundle.pain => learning.pain as f64,
            bundle.importance => learning.importance as f64,
            bundle.recurrence => 1u64,
        ))?;
    }
    writer.commit()?;
    drop(writer);
    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: stats
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Stats {
    min_ms: f64,
    mean_ms: f64,
    max_ms: f64,
}

fn stats_of(samples: &[Duration]) -> Stats {
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min_ms = ms.first().copied().unwrap_or(0.0);
    let max_ms = ms.last().copied().unwrap_or(0.0);
    let mean_ms = ms.iter().sum::<f64>() / ms.len() as f64;
    Stats {
        min_ms,
        mean_ms,
        max_ms,
    }
}

// ---------------------------------------------------------------------------
// Measurement #2 — IndexWriter open cost
// ---------------------------------------------------------------------------

fn measure_warm_open(index_path: &Path, samples: usize) -> Vec<Duration> {
    let mut out = Vec::with_capacity(samples);
    for _ in 0..samples {
        let t0 = Instant::now();
        let index = Index::open_in_dir(index_path).expect("open index");
        let writer: IndexWriter = index.writer(WRITER_HEAP_BYTES).expect("writer");
        let elapsed = t0.elapsed();
        drop(writer);
        drop(index);
        out.push(elapsed);
    }
    out
}

fn measure_cold_open(index_path: &Path, samples: usize) -> Vec<Duration> {
    let exe = std::env::current_exe().expect("current_exe");
    let mut out = Vec::with_capacity(samples);
    for _ in 0..samples {
        let output = Command::new(&exe)
            .arg("cold-open")
            .arg(index_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn cold-open child");
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            panic!(
                "cold-open child failed: status={:?} stderr={err}",
                output.status
            );
        }
        let s = String::from_utf8_lossy(&output.stdout);
        let ms: f64 = s.trim().parse().unwrap_or_else(|_| {
            panic!("cold-open child stdout not a float: {s:?}");
        });
        out.push(Duration::from_secs_f64(ms / 1000.0));
    }
    out
}

// ---------------------------------------------------------------------------
// Child modes
// ---------------------------------------------------------------------------

fn run_cold_open_child(index_path: &Path) -> ExitCode {
    let t0 = Instant::now();
    let index = match Index::open_in_dir(index_path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("cold-open child: open failed: {e}");
            return ExitCode::from(2);
        }
    };
    let writer: Result<IndexWriter, _> = index.writer(WRITER_HEAP_BYTES);
    match writer {
        Ok(_w) => {
            let elapsed = t0.elapsed();
            println!("{}", elapsed.as_secs_f64() * 1000.0);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cold-open child: writer failed: {e}");
            ExitCode::from(3)
        }
    }
}

fn run_writer_stall_child(index_path: &Path) -> ExitCode {
    let index = match Index::open_in_dir(index_path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("writer-stall child: open failed: {e}");
            return ExitCode::from(2);
        }
    };
    let _writer: IndexWriter = match index.writer(WRITER_HEAP_BYTES) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("writer-stall child: writer failed: {e}");
            return ExitCode::from(3);
        }
    };
    // Signal readiness so parent doesn't SIGKILL before we hold the lock.
    println!("LOCK_HELD");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    // Sleep until SIGKILLed by parent.
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn run_reader_reload_child(index_path: &Path) -> ExitCode {
    let index = match Index::open_in_dir(index_path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("reader-reload child: open failed: {e}");
            return ExitCode::from(2);
        }
    };
    let reader = match index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("reader-reload child: reader builder failed: {e}");
            return ExitCode::from(3);
        }
    };
    let t0 = Instant::now();
    if let Err(e) = tantivy::IndexReader::reload(&reader) {
        eprintln!("reader-reload child: reload failed: {e}");
        return ExitCode::from(4);
    }
    let elapsed = t0.elapsed();
    let searcher = reader.searcher();
    let count = searcher.search(&AllQuery, &Count).unwrap_or(usize::MAX);
    println!("{} {}", elapsed.as_secs_f64() * 1000.0, count);
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Measurement #3 — multi-process reader reload
// ---------------------------------------------------------------------------

fn measure_reader_reload(
    index_path: &Path,
) -> Result<(f64, usize, usize), Box<dyn std::error::Error>> {
    // Parent opens writer, adds N new docs, commits. Child re-execs and reloads.
    let bundle = build_schema();
    let index = Index::open_in_dir(index_path)?;

    // First, capture the baseline doc count via a child reload.
    let baseline_count = {
        let exe = std::env::current_exe()?;
        let out = Command::new(&exe)
            .arg("child-reader-reload")
            .arg(index_path)
            .output()?;
        if !out.status.success() {
            return Err(format!(
                "baseline reload child failed: {:?} {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            )
            .into());
        }
        let s = String::from_utf8_lossy(&out.stdout);
        let count_str = s.split_whitespace().nth(1).unwrap_or("0");
        count_str.parse::<usize>().unwrap_or(0)
    };

    // Add 100 new docs and commit.
    let mut writer: IndexWriter = index.writer(WRITER_HEAP_BYTES)?;
    let mut rng = Lcg(0xFEEDFACE);
    for i in 0..100u64 {
        let learning = synth_learning(1_000_000 + i, &mut rng);
        writer.add_document(doc!(
            bundle.content => learning.content.clone(),
            bundle.timestamp_sec => learning.timestamp.timestamp() as u64,
            bundle.pain => learning.pain as f64,
            bundle.importance => learning.importance as f64,
            bundle.recurrence => 1u64,
        ))?;
    }
    writer.commit()?;
    drop(writer);
    drop(index);

    // Now spawn a child reader and measure reload time + visible count.
    let exe = std::env::current_exe()?;
    let out = Command::new(&exe)
        .arg("child-reader-reload")
        .arg(index_path)
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "reload child failed: {:?} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = s.split_whitespace().collect();
    let elapsed_ms: f64 = parts[0].parse()?;
    let visible_count: usize = parts.get(1).map(|p| p.parse().unwrap_or(0)).unwrap_or(0);
    Ok((elapsed_ms, baseline_count, visible_count))
}

// ---------------------------------------------------------------------------
// Measurement #4 — lock cleanup under SIGKILL
// ---------------------------------------------------------------------------

struct LockObservation {
    lock_files_before_kill: Vec<String>,
    lock_files_after_kill: Vec<String>,
    fresh_writer_ok: bool,
    fresh_writer_error: Option<String>,
}

fn list_lock_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.contains("lock") || name.starts_with(".tantivy") {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

fn measure_sigkill_lock_cleanup(
    index_path: &Path,
) -> Result<LockObservation, Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new(&exe)
        .arg("child-writer-stall")
        .arg(index_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Wait for child to print LOCK_HELD, with a timeout.
    use std::io::{BufRead, BufReader};
    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    // Read one line; tantivy writer init should be fast.
    reader.read_line(&mut line)?;
    if !line.trim().eq("LOCK_HELD") {
        let _ = child.kill();
        return Err(format!("child did not print LOCK_HELD, got: {line:?}").into());
    }

    // Tiny extra pause to be safe.
    std::thread::sleep(Duration::from_millis(200));
    let lock_files_before_kill = list_lock_files(index_path);

    // SIGKILL: on Unix, std's Child::kill sends SIGKILL.
    child.kill()?;
    let _ = child.wait()?;
    // Give the OS a moment to reap.
    std::thread::sleep(Duration::from_millis(200));

    let lock_files_after_kill = list_lock_files(index_path);

    // Try to open a fresh IndexWriter from the parent.
    let (fresh_writer_ok, fresh_writer_error) = match Index::open_in_dir(index_path) {
        Ok(index) => match index.writer::<TantivyDocument>(WRITER_HEAP_BYTES) {
            Ok(_) => (true, None),
            Err(e) => (false, Some(format!("{e}"))),
        },
        Err(e) => (false, Some(format!("open_in_dir: {e}"))),
    };

    Ok(LockObservation {
        lock_files_before_kill,
        lock_files_after_kill,
        fresh_writer_ok,
        fresh_writer_error,
    })
}

// ---------------------------------------------------------------------------
// Suite
// ---------------------------------------------------------------------------

fn run_suite() -> Result<(), Box<dyn std::error::Error>> {
    // 100k generates ~tens of thousands of docs through tantivy; should be
    // tractable on a laptop in well under 5 min. If it isn't, the report
    // notes the substitution.
    let sizes: &[u64] = &[1_000, 10_000, 100_000];
    let base = std::env::temp_dir().join(format!("dreamd-weg24-{}", std::process::id()));
    std::fs::create_dir_all(&base)?;
    println!("# WEG-24 tantivy spike — scratch base: {}", base.display());

    let mut per_size: Vec<(u64, PathBuf, Stats, Stats)> = Vec::new();

    for &n in sizes {
        let idx_path = base.join(format!("idx-{n}"));
        let t0 = Instant::now();
        build_corpus(&idx_path, n)?;
        let build_elapsed = t0.elapsed();
        println!(
            "[corpus] n={n} built in {:.1} ms at {}",
            build_elapsed.as_secs_f64() * 1000.0,
            idx_path.display()
        );

        let warm = measure_warm_open(&idx_path, 10);
        let warm_s = stats_of(&warm);
        let cold = measure_cold_open(&idx_path, 10);
        let cold_s = stats_of(&cold);
        println!(
            "[open] n={n} warm min={:.2} mean={:.2} max={:.2} ms",
            warm_s.min_ms, warm_s.mean_ms, warm_s.max_ms
        );
        println!(
            "[open] n={n} cold min={:.2} mean={:.2} max={:.2} ms",
            cold_s.min_ms, cold_s.mean_ms, cold_s.max_ms
        );
        per_size.push((n, idx_path, warm_s, cold_s));
    }

    // Reader-reload uses the 10k corpus to keep the test snappy.
    let reload_target = per_size
        .iter()
        .find(|(n, _, _, _)| *n == 10_000)
        .map(|(_, p, _, _)| p.clone())
        .expect("10k corpus");
    let (reload_ms, baseline_count, visible_count) = measure_reader_reload(&reload_target)?;
    println!(
        "[reload] elapsed={:.2} ms baseline_count={} visible_count={}",
        reload_ms, baseline_count, visible_count
    );

    // SIGKILL lock cleanup also uses 10k (smallest non-trivial corpus that's still realistic).
    let lock_target = per_size
        .iter()
        .find(|(n, _, _, _)| *n == 10_000)
        .map(|(_, p, _, _)| p.clone())
        .expect("10k corpus");
    let lock_obs = measure_sigkill_lock_cleanup(&lock_target)?;
    println!(
        "[sigkill] lock_files_before_kill={:?}",
        lock_obs.lock_files_before_kill
    );
    println!(
        "[sigkill] lock_files_after_kill={:?}",
        lock_obs.lock_files_after_kill
    );
    println!("[sigkill] fresh_writer_ok={}", lock_obs.fresh_writer_ok);
    if let Some(e) = &lock_obs.fresh_writer_error {
        println!("[sigkill] fresh_writer_error={e}");
    }

    // Emit a machine-readable summary block at the end so the human can paste
    // numbers into the report file.
    println!("\n=== SUMMARY ===");
    for (n, _p, warm, cold) in &per_size {
        println!(
            "size={n} warm_min={:.2} warm_mean={:.2} warm_max={:.2} cold_min={:.2} cold_mean={:.2} cold_max={:.2}",
            warm.min_ms, warm.mean_ms, warm.max_ms, cold.min_ms, cold.mean_ms, cold.max_ms
        );
    }
    println!("reload_ms={:.2}", reload_ms);
    println!("baseline_count={}", baseline_count);
    println!("visible_count={}", visible_count);
    println!("lock_files_after_kill={:?}", lock_obs.lock_files_after_kill);
    println!("fresh_writer_ok={}", lock_obs.fresh_writer_ok);
    if let Some(e) = lock_obs.fresh_writer_error {
        println!("fresh_writer_error={e}");
    }

    // Decision line is computed by the human from warm mean @ 10k; we still
    // print the relevant scalar to make the report write trivial.
    if let Some((_, _, warm, _)) = per_size.iter().find(|(n, _, _, _)| *n == 10_000) {
        println!("decision_warm_mean_10k_ms={:.2}", warm.mean_ms);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.len() {
        1 => match run_suite() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("suite failed: {e}");
                ExitCode::from(1)
            }
        },
        3 => {
            let mode = args[1].as_str();
            let path = PathBuf::from(&args[2]);
            match mode {
                "cold-open" => run_cold_open_child(&path),
                "child-writer-stall" => run_writer_stall_child(&path),
                "child-reader-reload" => run_reader_reload_child(&path),
                other => {
                    eprintln!("unknown mode: {other}");
                    ExitCode::from(64)
                }
            }
        }
        _ => {
            eprintln!(
                "usage: tantivy_spike            (run full suite)\n       tantivy_spike cold-open <idx>\n       tantivy_spike child-writer-stall <idx>\n       tantivy_spike child-reader-reload <idx>"
            );
            ExitCode::from(64)
        }
    }
}
