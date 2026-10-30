//! Autosnap retention (BZR-1245).
//!
//! Every dream cycle writes one `snap-*` ref and the object it names
//! ([`crate::snapshot::stage_autosnap`]). Without a cap both grow forever. A
//! cycle therefore keeps the newest [`AUTOSNAP_KEEP`] `snap-*` refs, plus the
//! ref `branches/HEAD` names and the refs an in-progress bisect still needs.
//! It stages the unlinks of the rest ([`plan_ref_prune`]), so they commit with
//! the cycle or not at all. After the commit it removes the objects no ref
//! names ([`collect_objects`]). Named branches are never pruned.
//!
//! Both functions fail closed: when `HEAD`, the bisect state or a ref cannot
//! be read or parsed, they prune and remove nothing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::bisect::{self, BisectError};
use crate::layout::AgentRoot;
use crate::snapshot::{AUTOSNAP_REF_PREFIX, HEAD_FILE};

/// How many `snap-*` refs a dream cycle keeps (BZR-1245; user decision 2026-10-28).
pub const AUTOSNAP_KEEP: usize = 20;
/// An object younger than this is never collected: `memory branch` writes its
/// object before its ref, without the writer lock.
pub const OBJECT_GC_GRACE: Duration = Duration::from_secs(3600);

#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    /// Reading, listing or removing a path under `branches/` failed.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// `branches/HEAD` names neither a ref nor an object id.
    #[error("HEAD {path}: line 1 is neither `ref: refs/<name>` nor a 64-hex object id")]
    BadHead { path: PathBuf },
    /// A ref file's first line is not an object id.
    #[error("ref {path}: line 1 is not a 64-hex object id")]
    BadRef { path: PathBuf },
    /// The `branches/bisect` state could not be read.
    #[error("bisect state: {0}")]
    Bisect(#[from] BisectError),
}

/// Log a retention step that failed; a committed cycle never fails on one.
/// `commit` calls this on both sides of the commit, so there is one
/// `tracing::warn!` site, not two (each site counts against the release size
/// gate).
pub(crate) fn warn_skipped(e: &RetentionError, what: &str) {
    tracing::warn!(error = %e, "{what}");
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> RetentionError + '_ {
    move |source| RetentionError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn is_object_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `snap-` + 8 digits + `t` + 6 digits + `z`, exactly: the names
/// [`crate::snapshot::autosnap_ref_name`] writes.
fn is_autosnap_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(AUTOSNAP_REF_PREFIX) else {
        return false;
    };
    let b = rest.as_bytes();
    b.len() == 16
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b't'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'z'
}

/// What line 1 of `branches/HEAD` names.
enum Head {
    Absent,
    Attached(String),
    Detached(String),
}

/// Read `branches/HEAD`. An unreadable file, or a line 1 that is neither
/// `ref: refs/<name>` nor a 64-hex id, is an error.
fn read_head(agent_root: &AgentRoot) -> Result<Head, RetentionError> {
    let path = agent_root.branches_dir().join(HEAD_FILE);
    let body = match fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Head::Absent),
        Err(e) => return Err(io_err(&path)(e)),
    };
    let line = body.lines().next().unwrap_or("");
    match line.strip_prefix("ref: refs/") {
        Some(name) if !name.is_empty() => Ok(Head::Attached(name.to_string())),
        None if is_object_id(line) => Ok(Head::Detached(line.to_string())),
        _ => Err(RetentionError::BadHead { path }),
    }
}

/// The `snap-*` refs to unlink so that, once `staged` lands, the newest
/// [`AUTOSNAP_KEEP`] remain. Pure apart from reading `refs/`, `HEAD` and
/// `bisect`.
///
/// `staged` always takes one of the [`AUTOSNAP_KEEP`] slots, wherever its name
/// sorts, so a clock that runs backwards never grows the count. The ref `HEAD`
/// names and the refs an in-progress bisect names are kept on top of those
/// slots, and never push another ref out. Returned oldest first.
pub fn plan_ref_prune(agent_root: &AgentRoot, staged: &str) -> Result<Vec<String>, RetentionError> {
    let refs = agent_root.branches_refs_dir();
    let entries = match fs::read_dir(&refs) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(&refs)(e)),
    };
    // Kept sorted and unique as they are read. Name order is time order: the
    // newest are last.
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err(&refs))?;
        let path = entry.path();
        if !entry.file_type().map_err(io_err(&path))?.is_file() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if is_autosnap_name(name) {
                insert_sorted(&mut candidates, name);
            }
        }
    }
    // Counted once when a live ref has the same name.
    insert_sorted(&mut candidates, staged);

    let mut keep: Vec<String> = candidates
        .iter()
        .rev()
        .filter(|name| name.as_str() != staged)
        .take(AUTOSNAP_KEEP.saturating_sub(1))
        .cloned()
        .collect();
    keep.push(staged.to_string());
    if let Head::Attached(name) = read_head(agent_root)? {
        keep.push(name);
    }
    keep.extend(bisect::pinned_ref_names(agent_root)?);

    Ok(candidates
        .into_iter()
        .filter(|name| !keep.contains(name))
        .collect())
}

/// Insert `name` into the sorted, duplicate-free `names` unless it is there.
/// Not a `BTreeSet`: nothing else in the binary sorts `String`s, and the
/// generic code one linked here cost 17,632 bytes against the release size
/// gate (BZR-1245).
fn insert_sorted(names: &mut Vec<String>, name: &str) {
    if let Err(at) = names.binary_search_by(|n| n.as_str().cmp(name)) {
        names.insert(at, name.to_string());
    }
}

/// Remove every complete object no ref and no detached `HEAD` names, whose
/// directory mtime is before `cutoff`. Returns how many it removed.
///
/// Every file in `refs/` is read, dot-prefixed temp files included, and each
/// must name a 64-hex id on line 1; otherwise nothing is removed. Only
/// directories named exactly 64 lowercase hex are candidates, so a `.partial`
/// write in progress is never touched. Objects share files through hard links,
/// so removing one removes only its own links.
pub fn collect_objects(
    agent_root: &AgentRoot,
    cutoff: SystemTime,
) -> Result<usize, RetentionError> {
    // A `Vec`, not a `HashSet`: there are few refs, and it links less code.
    let mut named = Vec::new();
    let refs = agent_root.branches_refs_dir();
    match fs::read_dir(&refs) {
        Ok(entries) => {
            for entry in entries {
                let path = entry.map_err(io_err(&refs))?.path();
                let body = fs::read_to_string(&path).map_err(io_err(&path))?;
                let id = body.lines().next().unwrap_or("");
                if !is_object_id(id) {
                    return Err(RetentionError::BadRef { path });
                }
                named.push(id.to_string());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(&refs)(e)),
    }
    if let Head::Detached(id) = read_head(agent_root)? {
        named.push(id);
    }

    let objects = agent_root.branches_objects_dir();
    let entries = match fs::read_dir(&objects) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io_err(&objects)(e)),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry.map_err(io_err(&objects))?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !is_object_id(&name) || named.contains(&name) {
            continue;
        }
        let meta = entry.metadata().map_err(io_err(&path))?;
        if !meta.is_dir() || meta.modified().map_err(io_err(&path))? >= cutoff {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path)(e)),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot;

    // 2026-09-29T14:03:00Z
    const T0: i64 = 1_790_690_580;

    fn scaffold() -> (tempfile::TempDir, AgentRoot) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = AgentRoot::new(dir.path());
        fs::create_dir_all(root.episodic_dir()).unwrap();
        fs::create_dir_all(root.semantic_dir()).unwrap();
        fs::create_dir_all(root.branches_refs_dir()).unwrap();
        (dir, root)
    }

    /// The autosnap ref name `i` minutes after `T0`.
    fn snap(i: i64) -> String {
        snapshot::autosnap_ref_name(T0 + i * 60).unwrap()
    }

    fn fake_id(i: u64) -> String {
        format!("{i:064x}")
    }

    fn write_ref(root: &AgentRoot, name: &str, id: &str) {
        fs::write(
            root.branches_refs_dir().join(name),
            format!("{id}\n2026-09-29T14:03:00Z\n"),
        )
        .unwrap();
    }

    /// `snap(i)` for each `i` in `range`, each naming its own fake id.
    /// Returns the names oldest first.
    fn live(root: &AgentRoot, range: std::ops::Range<i64>) -> Vec<String> {
        range
            .map(|i| {
                let name = snap(i);
                write_ref(root, &name, &fake_id(i as u64));
                name
            })
            .collect()
    }

    fn write_head(root: &AgentRoot, body: &str) {
        fs::write(root.branches_dir().join(HEAD_FILE), body).unwrap();
    }

    fn write_bisect(root: &AgentRoot, body: &str) {
        fs::write(bisect::state_path(root), body).unwrap();
    }

    /// An object dir holding one file, with its mtime set to `mtime`.
    fn object(root: &AgentRoot, name: &str, mtime: SystemTime) -> PathBuf {
        let dir = root.branches_objects_dir().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(snapshot::MANIFEST_FILE), b"{}\n").unwrap();
        age(&dir, mtime);
        dir
    }

    fn age(dir: &Path, mtime: SystemTime) {
        fs::File::open(dir).unwrap().set_modified(mtime).unwrap();
    }

    fn hours_ago(h: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(h * 3600)
    }

    #[test]
    fn plan_keeps_the_newest_twenty_snap_refs() {
        let (_dir, root) = scaffold();
        let names = live(&root, 0..25);

        let pruned = plan_ref_prune(&root, &snap(100)).unwrap();

        assert_eq!(pruned, names[..6].to_vec());
    }

    #[test]
    fn plan_counts_a_same_second_ref_once() {
        let (_dir, root) = scaffold();
        let names = live(&root, 0..20);

        // The newest live ref has the staged name: 20 candidates, not 21.
        let pruned = plan_ref_prune(&root, &names[19]).unwrap();

        assert!(pruned.is_empty(), "{pruned:?}");
    }

    #[test]
    fn plan_never_prunes_the_staged_ref() {
        let (_dir, root) = scaffold();
        let names = live(&root, 10..30);
        let staged = snap(0);

        let pruned = plan_ref_prune(&root, &staged).unwrap();

        assert_eq!(pruned, vec![names[0].clone()]);
        assert!(!pruned.contains(&staged));
    }

    #[test]
    fn plan_keeps_the_ref_head_names() {
        let (_dir, root) = scaffold();
        let names = live(&root, 0..21);
        write_head(&root, &format!("ref: refs/{}\n", names[0]));

        let pruned = plan_ref_prune(&root, &snap(100)).unwrap();

        assert_eq!(pruned, vec![names[1].clone()]);
    }

    #[test]
    fn plan_keeps_refs_an_in_progress_bisect_names() {
        let (_dir, root) = scaffold();
        let names = live(&root, 0..23);
        write_bisect(
            &root,
            &format!(
                r#"{{"good_idx":0,"bad_idx":2,"current_idx":1,"steps":["{}","{}","{}"]}}"#,
                names[0], names[1], names[2]
            ),
        );

        let pruned = plan_ref_prune(&root, &snap(100)).unwrap();

        assert_eq!(pruned, vec![names[3].clone()]);
    }

    #[test]
    fn plan_prunes_nothing_when_bisect_state_is_malformed() {
        let (_dir, root) = scaffold();
        live(&root, 0..25);
        write_bisect(&root, "{not json");

        let result = plan_ref_prune(&root, &snap(100));

        assert!(
            matches!(result, Err(RetentionError::Bisect(_))),
            "{result:?}"
        );
    }

    #[test]
    fn plan_prunes_nothing_when_head_is_unparseable() {
        let (_dir, root) = scaffold();
        live(&root, 0..25);
        write_head(&root, "junk\n");

        let result = plan_ref_prune(&root, &snap(100));

        assert!(
            matches!(result, Err(RetentionError::BadHead { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn plan_never_touches_named_or_odd_snap_refs() {
        let (_dir, root) = scaffold();
        let names = live(&root, 0..25);
        let odd = ["exp", "snap-foo", "snap-20260101t000000Z"];
        for name in odd {
            write_ref(&root, name, &fake_id(999));
        }

        let pruned = plan_ref_prune(&root, &snap(100)).unwrap();

        for name in odd {
            assert!(!pruned.iter().any(|p| p == name), "{name} pruned");
        }
        assert_eq!(pruned, names[..6].to_vec());
    }

    #[test]
    fn gc_removes_only_unreferenced_objects_older_than_the_cutoff() {
        let (_dir, root) = scaffold();
        let a = object(&root, &fake_id(0xa), hours_ago(2));
        let b = object(&root, &fake_id(0xb), hours_ago(2));
        let c = object(&root, &fake_id(0xc), SystemTime::now());
        let d = object(&root, &format!("{}.partial", fake_id(0xd)), hours_ago(2));
        write_ref(&root, &snap(0), &fake_id(0xa));

        let removed = collect_objects(&root, hours_ago(1)).unwrap();

        assert_eq!(removed, 1);
        assert!(a.is_dir(), "a ref names A");
        assert!(!b.exists(), "B is unnamed and old");
        assert!(c.is_dir(), "C is inside the grace");
        assert!(d.is_dir(), "a .partial write is never touched");
    }

    #[test]
    fn gc_keeps_the_object_a_detached_head_names() {
        let (_dir, root) = scaffold();
        let b = object(&root, &fake_id(0xb), hours_ago(2));
        write_ref(&root, &snap(0), &fake_id(0xa));
        write_head(&root, &format!("{}\n", fake_id(0xb)));

        let removed = collect_objects(&root, hours_ago(1)).unwrap();

        assert_eq!(removed, 0);
        assert!(b.is_dir());
    }

    /// A ref being written lands as a dot-prefixed temp file before its rename,
    /// so the object it names must survive (BZR-1245).
    #[test]
    fn gc_keeps_the_object_a_dot_prefixed_temp_ref_names() {
        let (_dir, root) = scaffold();
        let b = object(&root, &fake_id(0xb), hours_ago(2));
        write_ref(&root, ".exp.tmp", &fake_id(0xb));

        let removed = collect_objects(&root, hours_ago(1)).unwrap();

        assert_eq!(removed, 0);
        assert!(b.is_dir(), "only the temp ref names B");
    }

    #[test]
    fn gc_removes_nothing_when_a_ref_is_unreadable() {
        let (_dir, root) = scaffold();
        let b = object(&root, &fake_id(0xb), hours_ago(2));
        write_ref(&root, &snap(0), &fake_id(0xa));
        fs::write(root.branches_refs_dir().join("broken"), "junk\n").unwrap();

        let result = collect_objects(&root, hours_ago(1));

        assert!(
            matches!(result, Err(RetentionError::BadRef { .. })),
            "{result:?}"
        );
        assert!(b.is_dir());
    }

    #[test]
    fn gc_keeps_files_hardlinked_into_surviving_objects() {
        use sha2::{Digest, Sha256};

        let (_dir, root) = scaffold();
        fs::write(root.episodic_jsonl(), "{\"x\":1}\n").unwrap();
        fs::write(root.lessons_md(), "a\n").unwrap();
        let id_a = snapshot::create_autosnap(&root, T0).unwrap();
        fs::write(root.lessons_md(), "b\n").unwrap();
        let id_b = snapshot::create_autosnap(&root, T0 + 60).unwrap();
        assert_ne!(id_a, id_b);
        fs::remove_file(root.branches_refs_dir().join(snap(1))).unwrap();
        let obj_a = root.branches_objects_dir().join(&id_a);
        let obj_b = root.branches_objects_dir().join(&id_b);
        let shared = "episodic/AGENT_LEARNINGS.jsonl";
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(obj_a.join(shared)).unwrap().nlink(),
                2,
                "B's log must be a hard link to A's"
            );
        }
        age(&obj_a, hours_ago(2));
        age(&obj_b, hours_ago(2));

        let removed = collect_objects(&root, hours_ago(1)).unwrap();

        assert_eq!(removed, 1);
        assert!(!obj_b.exists());
        let manifest: snapshot::Manifest =
            serde_json::from_slice(&fs::read(obj_a.join(snapshot::MANIFEST_FILE)).unwrap())
                .unwrap();
        assert!(manifest.files.iter().any(|f| f.path == shared));
        for file in manifest.files {
            let bytes = fs::read(obj_a.join(&file.path)).unwrap();
            let hex: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(hex, file.sha256, "{}", file.path);
        }
    }
}
