//! Staged dream-cycle outputs (BZR-1239).
//!
//! A dream cycle (or a forget) writes every output it produces into
//! `.agent/.dreamd/stage/<kind>-<now_sec>/` and fsyncs it there. Nothing live
//! changes while the cycle stages. [`crate::wal::commit_cycle`] then writes one
//! redo WAL naming every rename and unlink (`Stage::redo`) — the commit point
//! — and applies it. A crash before the commit point leaves only the stage
//! directory, which the next writer to take the project lock wipes.
//!
//! A cycle plans every output first ([`crate::plan`]) and stages the plan with
//! [`Stage::apply_plan`], which also carries the plan's ledger edges into the
//! redo WAL. No phase reads back an output another staged: the cycle's
//! inputs (the log, `LESSONS.md`) are paths nothing has staged when it reads
//! them, so those reads return the live files. Other reads only ask what the
//! commit will leave: `unlink` checks whether its path will exist, and
//! `apply_plan` reads the day's archive.
//!
//! Test builds also carry the crash hooks the crash sweep drives
//! (`crash_point`, `arm_crash`, `crash_fired`). Release builds compile
//! `crash_point` to `Ok(())`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::decay;
use crate::layout::AgentRoot;
use crate::lessons;
use crate::plan::{CyclePlan, LessonsChange};
use crate::provenance::Kind;
use crate::wal::{RedoEdge, RedoRename, RedoWal, REDO_FORMAT, STATE_SCHEMA_VERSION};

/// A dream cycle's (or forget's) outputs, staged under
/// `.agent/.dreamd/stage/<name>/` before the redo WAL commits them.
#[derive(Debug)]
pub struct Stage {
    agent_root: AgentRoot,
    dir: PathBuf,
    seq: u32,
    /// Key: live path relative to `.agent/`.
    entries: BTreeMap<PathBuf, Entry>,
    /// Ledger edges the commit appends, in order.
    edges: Vec<(Kind, String, String)>,
}

#[derive(Debug)]
enum Entry {
    /// Staged file name inside `dir`.
    Put(String),
    Unlink,
}

impl Stage {
    /// Start an empty stage at `.dreamd/stage/<name>/`, replacing any
    /// directory a crashed run of the same name left there.
    pub(crate) fn create(agent_root: &AgentRoot, name: &str) -> io::Result<Stage> {
        let dir = agent_root.stage_dir().join(name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fs::create_dir_all(&dir)?;
        Ok(Stage {
            agent_root: agent_root.clone(),
            dir,
            seq: 0,
            entries: BTreeMap::new(),
            edges: Vec::new(),
        })
    }

    pub fn agent_root(&self) -> &AgentRoot {
        &self.agent_root
    }

    /// The bytes `live` will hold once this stage commits: the staged bytes
    /// for a [`put`](Self::put), `None` for an [`unlink`](Self::unlink), and
    /// otherwise the live file (`None` when it is absent).
    ///
    /// A cycle's inputs are paths nothing has staged when it reads them, so
    /// those reads return the live files. A dream cycle stages its autosnap
    /// ref and the old refs' unlinks before it reads `LESSONS.md`; each
    /// `unlink` reads through the stage to skip a ref that is already gone.
    /// [`apply_plan`](Self::apply_plan) reads the day's archive and, through
    /// `unlink`, whether `LESSONS.md` exists.
    pub fn read(&self, live: &Path) -> io::Result<Option<Vec<u8>>> {
        if let Some(rel) = self.relative(live) {
            match self.entries.get(&rel) {
                Some(Entry::Put(name)) => return fs::read(self.dir.join(name)).map(Some),
                Some(Entry::Unlink) => return Ok(None),
                None => {}
            }
        }
        match fs::read(live) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Stage `bytes` as the new contents of `live`. The staged file is
    /// written under a fresh name and fsynced before it is recorded. A later
    /// `put` of the same path replaces the entry; the earlier staged file
    /// stays until the stage is wiped.
    ///
    /// `live` must be under `.agent/`, or this is
    /// [`io::ErrorKind::InvalidInput`]. The staged file is fsynced here. The
    /// directory entry becomes durable at the commit flush (`crate::io::sync_dir`).
    pub fn put(&mut self, live: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;

        let rel = self.checked_relative(live)?;
        let file_name = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = format!("{}-{file_name}", self.seq);
        self.seq += 1;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.dir.join(&name))?;
        file.write_all(bytes)?;
        file.sync_data()?;
        drop(file);
        crash_point()?;
        self.entries.insert(rel, Entry::Put(name));
        Ok(())
    }

    /// Stage the removal of `live`. Records nothing when `live` would not
    /// exist after this stage commits, so retiring an absent `LESSONS.md`
    /// stages nothing.
    pub fn unlink(&mut self, live: &Path) -> io::Result<()> {
        let rel = self.checked_relative(live)?;
        if self.read(live)?.is_some() {
            self.entries.insert(rel, Entry::Unlink);
        }
        Ok(())
    }

    /// Add `edges` to the ledger edges the commit appends, after any already
    /// recorded.
    pub fn record_edges(&mut self, edges: &[(Kind, String, String)]) {
        self.edges.extend_from_slice(edges);
    }

    /// Stage `plan`: the log, the `LESSONS.md` write or unlink, the sidecar,
    /// and, when anything decays, the day's archive merged with its current
    /// bytes ([`decay::merge_archive`]). Then record the plan's edges.
    /// `cycle_date` names the archive (`YYYY-MM-DD`).
    pub fn apply_plan(&mut self, plan: &CyclePlan, cycle_date: &str) -> io::Result<()> {
        let agent_root = self.agent_root.clone();
        if let Some(log) = &plan.log {
            self.put(&agent_root.episodic_jsonl(), log)?;
        }
        match &plan.lessons {
            LessonsChange::Keep => {}
            LessonsChange::Write(file) => self.put(
                &agent_root.lessons_md(),
                lessons::render_lessons_file(file).as_bytes(),
            )?,
            LessonsChange::Unlink => self.unlink(&agent_root.lessons_md())?,
        }
        if let Some(sidecar) = &plan.sidecar {
            self.put(
                &agent_root.semantic_dir().join("recurrence_counts.json"),
                sidecar,
            )?;
        }
        if !plan.archived.is_empty() {
            let archive_path = agent_root.snapshot_file(cycle_date);
            let archive = decay::merge_archive(self.read(&archive_path)?, &plan.archived);
            self.put(&archive_path, &archive)?;
        }
        self.record_edges(&plan.edges);
        Ok(())
    }

    /// The redo WAL that commits this stage: `from` is the staged file
    /// (`.dreamd/stage/<name>/<file>`), `to` and `unlinks` are live paths, all
    /// relative to `.agent/` and `/`-joined, in path order. `edges` are the
    /// recorded ledger edges, in the order they were recorded.
    pub(crate) fn redo(&self) -> RedoWal {
        let stage_rel = self
            .relative(&self.dir)
            .map(|rel| slash_joined(&rel))
            .unwrap_or_default();
        let mut renames = Vec::new();
        let mut unlinks = Vec::new();
        for (rel, entry) in &self.entries {
            match entry {
                Entry::Put(name) => renames.push(RedoRename {
                    from: format!("{stage_rel}/{name}"),
                    to: slash_joined(rel),
                }),
                Entry::Unlink => unlinks.push(slash_joined(rel)),
            }
        }
        RedoWal {
            schema_version: STATE_SCHEMA_VERSION.to_string(),
            format: REDO_FORMAT.to_string(),
            renames,
            unlinks,
            edges: self
                .edges
                .iter()
                .map(|(kind, from, to)| RedoEdge {
                    kind: *kind,
                    from: from.clone(),
                    to: to.clone(),
                })
                .collect(),
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// `live` relative to `.agent/`, lexically.
    fn relative(&self, live: &Path) -> Option<PathBuf> {
        live.strip_prefix(self.agent_root.agent_dir())
            .ok()
            .map(Path::to_path_buf)
    }

    /// [`relative`](Self::relative), refusing anything that is not a plain
    /// path below `.agent/`.
    fn checked_relative(&self, live: &Path) -> io::Result<PathBuf> {
        self.relative(live)
            .filter(|rel| {
                rel.components().next().is_some()
                    && rel.components().all(|c| matches!(c, Component::Normal(_)))
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} is not under {}",
                        live.display(),
                        self.agent_root.agent_dir().display()
                    ),
                )
            })
    }
}

fn slash_joined(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
thread_local! {
    static CRASH: std::cell::Cell<(Option<u32>, bool)> = const { std::cell::Cell::new((None, false)) };
}

/// Test builds: fail the call that finds the countdown at 0, and every call
/// after it. Release builds: always `Ok`.
#[cfg(test)]
pub(crate) fn crash_point() -> io::Result<()> {
    CRASH.with(|c| match c.get() {
        (Some(0), _) => {
            c.set((Some(0), true));
            Err(io::Error::other("injected crash"))
        }
        (Some(n), fired) => {
            c.set((Some(n - 1), fired));
            Ok(())
        }
        (None, _) => Ok(()),
    })
}
#[cfg(not(test))]
#[inline(always)]
pub(crate) fn crash_point() -> io::Result<()> {
    Ok(())
}

/// `Some(k)`: the k-th `crash_point` call from now (0-based) fails. `None` disarms.
#[cfg(test)]
pub(crate) fn arm_crash(after: Option<u32>) {
    CRASH.with(|c| c.set((after, false)));
}
/// Whether a `crash_point` call has failed since the last `arm_crash`.
#[cfg(test)]
pub(crate) fn crash_fired() -> bool {
    CRASH.with(|c| c.get().1)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{unique_tmpdir, DirGuard};

    fn store(label: &str) -> (AgentRoot, DirGuard) {
        let dir = unique_tmpdir(label);
        let root = AgentRoot::new(&dir);
        fs::create_dir_all(root.semantic_dir()).unwrap();
        (root, DirGuard(dir))
    }

    #[test]
    fn read_sees_a_put_and_an_unlink() {
        let (root, _g) = store("stage-read");
        let lessons = root.lessons_md();
        let sidecar = root.semantic_dir().join("recurrence_counts.json");
        fs::write(&lessons, b"live lessons").unwrap();
        fs::write(&sidecar, b"live sidecar").unwrap();
        let mut stage = Stage::create(&root, "dream-1").unwrap();

        stage.put(&sidecar, b"staged sidecar").unwrap();
        stage.unlink(&lessons).unwrap();

        assert_eq!(stage.read(&sidecar).unwrap().unwrap(), b"staged sidecar");
        assert_eq!(stage.read(&lessons).unwrap(), None);
        assert_eq!(fs::read(&sidecar).unwrap(), b"live sidecar");
        assert_eq!(fs::read(&lessons).unwrap(), b"live lessons");
        assert_eq!(
            stage.read(&root.episodic_jsonl()).unwrap(),
            None,
            "an absent live file with no entry reads as None"
        );
        let redo = stage.redo();
        assert_eq!(
            redo.renames,
            vec![RedoRename {
                from: ".dreamd/stage/dream-1/0-recurrence_counts.json".to_string(),
                to: "semantic/recurrence_counts.json".to_string(),
            }]
        );
        assert_eq!(redo.unlinks, vec!["semantic/LESSONS.md".to_string()]);
    }

    #[test]
    fn a_second_put_keeps_the_last_bytes_under_a_new_name() {
        let (root, _g) = store("stage-second-put");
        let jsonl = root.episodic_jsonl();
        let mut stage = Stage::create(&root, "dream-2").unwrap();

        stage.put(&jsonl, b"first\n").unwrap();
        stage.put(&jsonl, b"second\n").unwrap();

        assert_eq!(stage.read(&jsonl).unwrap().unwrap(), b"second\n");
        let redo = stage.redo();
        assert_eq!(redo.renames.len(), 1);
        assert_eq!(
            redo.renames[0].from,
            ".dreamd/stage/dream-2/1-AGENT_LEARNINGS.jsonl"
        );
        assert_eq!(
            fs::read(stage.dir().join("0-AGENT_LEARNINGS.jsonl")).unwrap(),
            b"first\n",
            "the earlier staged file stays until the wipe"
        );
        assert!(!jsonl.exists(), "nothing live changes while staging");
    }

    #[test]
    fn put_outside_the_agent_dir_is_refused() {
        let (root, _g) = store("stage-outside");
        let mut stage = Stage::create(&root, "dream-3").unwrap();
        let outside = root.project_root().join("README.md");
        let dotdot = root.agent_dir().join("semantic/../../escape.md");

        for path in [&outside, &dotdot] {
            let err = stage.put(path, b"nope").unwrap_err();
            assert_eq!(
                err.kind(),
                io::ErrorKind::InvalidInput,
                "{}",
                path.display()
            );
        }
        assert!(stage.redo().renames.is_empty());
        assert_eq!(fs::read_dir(stage.dir()).unwrap().count(), 0);
        assert!(!outside.exists());
    }

    #[test]
    fn unlink_of_an_absent_file_stages_nothing() {
        let (root, _g) = store("stage-unlink-absent");
        let mut stage = Stage::create(&root, "dream-4").unwrap();

        stage.unlink(&root.lessons_md()).unwrap();

        assert!(stage.redo().unlinks.is_empty());
        assert!(stage.redo().renames.is_empty());
    }
}
