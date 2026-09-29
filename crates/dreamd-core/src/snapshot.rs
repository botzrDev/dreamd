//! Content-addressed memory objects and autosnap refs (BZR-160).
//!
//! Format is `docs/branching.md` (`branches/1.0`, BZR-159). An object is
//! `.agent/.dreamd/branches/objects/<id>/`, where `<id>` is the lowercase-hex
//! SHA-256 of the canonical manifest bytes, trailing newline included. The
//! manifest carries no id and no timestamp; when an object was taken is
//! recorded on the ref that names it, `branches/refs/snap-<yyyymmddthhmmssz>`.
//!
//! The decay archive directory is a different tree and is never written here.
//!
//! Named branches (BZR-150): [`create_branch`] points `refs/<name>` at a fresh
//! object and attaches `branches/HEAD`; [`checkout`] copies an object's bytes
//! back over the live files; [`delete_branch`] removes a ref, never an object.
//! [`create_autosnap`] alone never writes HEAD. There is one live store: learns
//! keep appending to the live JSONL, and no ref moves when they do.
//!
//! Object bytes are copies of the live files, read once and hashed from the
//! same buffer that is written. A live path is never hardlinked (the episodic
//! log is held open by the coordinator and keeps growing). Dedup hardlinks only
//! from a file inside another complete object with the same `sha256`, and falls
//! back to writing the bytes when the link fails.
//!
//! Writes use plain `std::fs::write` + `std::fs::rename`, so this module also
//! runs on Windows, where the crate's atomic-write helper is unsupported.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::layout::AgentRoot;

/// `schema_version` of every manifest this module writes.
pub const BRANCHES_SCHEMA_VERSION: &str = "branches/1.0";

/// Manifest filename inside an object directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Prefix of the ref name every autosnap writes.
pub const AUTOSNAP_REF_PREFIX: &str = "snap-";

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("manifest serialize: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("timestamp {0} is out of range")]
    InvalidTimestamp(i64),
    #[error("invalid branch name {0:?}: expected [a-z0-9][a-z0-9._-]{{0,63}} with no `..`")]
    InvalidName(String),
    #[error("branch {0:?} already exists")]
    BranchExists(String),
    #[error("branch {0:?} not found")]
    BranchNotFound(String),
    #[error("branch {0:?} is checked out (HEAD); check out another branch first")]
    BranchIsHead(String),
    #[error("ref {path}: {reason}")]
    BadRef { path: PathBuf, reason: String },
    #[error("object {id}: {reason}")]
    BadObject { id: String, reason: String },
    #[error(
        "daemon socket {0} exists; stop `dreamd watch` before checkout \
         (it holds the live episodic log open)"
    )]
    DaemonRunning(PathBuf),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> SnapshotError + '_ {
    move |source| SnapshotError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Canonical manifest. Field order is the key order on the wire.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub schema_version: String,
    pub files: Vec<ManifestFile>,
}

/// One manifest entry. Field order is `path`, `sha256`, `size`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct ManifestFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// The four live files a manifest may list, as (`path` relative to `.agent/`,
/// live location). Sorted by `path` in [`read_live`], not here.
fn live_files(agent_root: &AgentRoot) -> [(&'static str, PathBuf); 4] {
    [
        (
            "episodic/AGENT_LEARNINGS.jsonl",
            agent_root.episodic_jsonl(),
        ),
        ("semantic/LESSONS.md", agent_root.lessons_md()),
        (
            "semantic/recurrence_counts.json",
            agent_root.semantic_dir().join("recurrence_counts.json"),
        ),
        ("personal/PREFERENCES.md", agent_root.preferences_md()),
    ]
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Read each live file once. Absent or unreadable → omitted, never an error.
/// Returned in `path` byte order with the bytes that were hashed.
fn read_live(agent_root: &AgentRoot) -> Vec<(ManifestFile, Vec<u8>)> {
    let mut out: Vec<(ManifestFile, Vec<u8>)> = live_files(agent_root)
        .into_iter()
        .filter_map(|(rel, live)| match fs::read(&live) {
            Ok(bytes) => Some((
                ManifestFile {
                    path: rel.to_string(),
                    sha256: sha256_hex(&bytes),
                    size: bytes.len() as u64,
                },
                bytes,
            )),
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(path = %live.display(), error = %e, "unreadable; omitted from autosnap");
                }
                None
            }
        })
        .collect();
    out.sort_by(|a, b| a.0.path.as_bytes().cmp(b.0.path.as_bytes()));
    out
}

/// Canonical manifest bytes: compact JSON plus one trailing `\n`.
pub fn manifest_bytes(files: &[ManifestFile]) -> Result<Vec<u8>, SnapshotError> {
    let mut sorted = files.to_vec();
    sorted.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let manifest = Manifest {
        schema_version: BRANCHES_SCHEMA_VERSION.to_string(),
        files: sorted,
    };
    let mut bytes = serde_json::to_vec(&manifest)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// `sha256` → a file inside an existing complete object with those bytes.
/// Unreadable or unparseable manifests contribute nothing; dedup is best-effort.
fn existing_object_files(objects_dir: &Path) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    let Ok(entries) = fs::read_dir(objects_dir) else {
        return map;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let is_complete = dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit()));
        if !is_complete {
            continue;
        }
        let Ok(raw) = fs::read(dir.join(MANIFEST_FILE)) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_slice::<Manifest>(&raw) else {
            continue;
        };
        for f in manifest.files {
            map.entry(f.sha256).or_insert_with(|| dir.join(&f.path));
        }
    }
    map
}

/// Write the object directory for `id` unless it already exists.
fn write_object(
    objects_dir: &Path,
    id: &str,
    files: &[(ManifestFile, Vec<u8>)],
    manifest: &[u8],
) -> Result<(), SnapshotError> {
    let final_dir = objects_dir.join(id);
    if final_dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(objects_dir).map_err(io_err(objects_dir))?;

    let partial = objects_dir.join(format!("{id}.partial"));
    // A leftover partial is a crashed write of these same bytes.
    if partial.exists() {
        fs::remove_dir_all(&partial).map_err(io_err(&partial))?;
    }
    let existing = existing_object_files(objects_dir);

    for (entry, bytes) in files {
        let dest = partial.join(&entry.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        let linked = existing
            .get(&entry.sha256)
            .is_some_and(|src| fs::hard_link(src, &dest).is_ok());
        if !linked {
            fs::write(&dest, bytes).map_err(io_err(&dest))?;
        }
    }
    let manifest_path = partial.join(MANIFEST_FILE);
    fs::write(&manifest_path, manifest).map_err(io_err(&manifest_path))?;

    if let Err(e) = fs::rename(&partial, &final_dir) {
        // Someone else completed the same id first: identical content.
        if final_dir.is_dir() {
            let _ = fs::remove_dir_all(&partial);
            return Ok(());
        }
        return Err(io_err(&final_dir)(e));
    }
    Ok(())
}

/// `snap-` + `YYYYMMDDThhmmssZ` lowercased, e.g. `snap-20260929t140300z`.
pub fn autosnap_ref_name(now_sec: i64) -> Result<String, SnapshotError> {
    let ts = chrono::DateTime::from_timestamp(now_sec, 0)
        .ok_or(SnapshotError::InvalidTimestamp(now_sec))?;
    Ok(format!(
        "{AUTOSNAP_REF_PREFIX}{}",
        ts.format("%Y%m%dT%H%M%SZ").to_string().to_ascii_lowercase()
    ))
}

fn write_ref(refs_dir: &Path, name: &str, id: &str, now_sec: i64) -> Result<(), SnapshotError> {
    let ts = chrono::DateTime::from_timestamp(now_sec, 0)
        .ok_or(SnapshotError::InvalidTimestamp(now_sec))?;
    fs::create_dir_all(refs_dir).map_err(io_err(refs_dir))?;
    let body = format!("{id}\n{}\n", ts.format("%Y-%m-%dT%H:%M:%SZ"));
    // Leading dot: the temp name cannot match the ref-name grammar.
    let tmp = refs_dir.join(format!(".{name}.tmp"));
    fs::write(&tmp, body).map_err(io_err(&tmp))?;
    let dest = refs_dir.join(name);
    fs::rename(&tmp, &dest).map_err(io_err(&dest))
}

/// Snapshot the four live memory files into a content-addressed object and
/// point `refs/snap-<now>` at it. Returns the object id.
///
/// `now_sec` is the caller's clock; this function never reads the wall clock.
/// A same-second ref is overwritten; the earlier object stays on disk.
pub fn create_autosnap(agent_root: &AgentRoot, now_sec: i64) -> Result<String, SnapshotError> {
    let name = autosnap_ref_name(now_sec)?;
    let files = read_live(agent_root);
    let entries: Vec<ManifestFile> = files.iter().map(|(m, _)| m.clone()).collect();
    let manifest = manifest_bytes(&entries)?;
    let id = sha256_hex(&manifest);

    write_object(&agent_root.branches_objects_dir(), &id, &files, &manifest)?;
    write_ref(&agent_root.branches_refs_dir(), &name, &id, now_sec)?;
    Ok(id)
}

/// Filename of the branch pointer inside `branches/`.
pub const HEAD_FILE: &str = "HEAD";

/// A row of [`list_branches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRow {
    pub name: String,
    pub id: String,
    /// `branches/HEAD` is `ref: refs/<name>`.
    pub current: bool,
}

/// `[a-z0-9][a-z0-9._-]{0,63}`, no `..`. The grammar already excludes `/`.
pub fn is_valid_branch_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|&c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
        && !name.contains("..")
}

fn validate_name(name: &str) -> Result<(), SnapshotError> {
    if is_valid_branch_name(name) {
        Ok(())
    } else {
        Err(SnapshotError::InvalidName(name.to_string()))
    }
}

fn is_object_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Object id on the first line of `refs/<name>`. A missing ref is
/// [`SnapshotError::BranchNotFound`].
fn read_ref_id(refs_dir: &Path, name: &str) -> Result<String, SnapshotError> {
    let path = refs_dir.join(name);
    let body = match fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(SnapshotError::BranchNotFound(name.to_string()))
        }
        Err(e) => return Err(io_err(&path)(e)),
    };
    let id = body.lines().next().unwrap_or("");
    if !is_object_id(id) {
        return Err(SnapshotError::BadRef {
            path,
            reason: "first line is not a 64-hex object id".to_string(),
        });
    }
    Ok(id.to_string())
}

/// The branch name HEAD is attached to, or `None` when HEAD is absent,
/// detached, or unreadable as a ref line.
fn head_branch(agent_root: &AgentRoot) -> Result<Option<String>, SnapshotError> {
    let path = agent_root.branches_dir().join(HEAD_FILE);
    match fs::read_to_string(&path) {
        Ok(body) => Ok(body
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("ref: refs/"))
            .map(str::to_string)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(&path)(e)),
    }
}

/// `std::fs::write` to a dot-prefixed sibling, then `rename` over `dest`.
fn write_replace(dest: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{file_name}.tmp"));
    fs::write(&tmp, bytes).map_err(io_err(&tmp))?;
    fs::rename(&tmp, dest).map_err(io_err(dest))
}

fn write_head(agent_root: &AgentRoot, name: &str) -> Result<(), SnapshotError> {
    let head = agent_root.branches_dir().join(HEAD_FILE);
    write_replace(&head, format!("ref: refs/{name}\n").as_bytes())
}

/// Snapshot the live files (via [`create_autosnap`]), point `refs/<name>` at
/// the object, and attach HEAD to it. Returns the object id.
///
/// An existing `refs/<name>` is an error and is left unchanged; no autosnap is
/// taken in that case. `now_sec` is the caller's clock.
pub fn create_branch(
    agent_root: &AgentRoot,
    name: &str,
    now_sec: i64,
) -> Result<String, SnapshotError> {
    validate_name(name)?;
    let refs_dir = agent_root.branches_refs_dir();
    if refs_dir.join(name).exists() {
        return Err(SnapshotError::BranchExists(name.to_string()));
    }
    let id = create_autosnap(agent_root, now_sec)?;
    write_ref(&refs_dir, name, &id, now_sec)?;
    write_head(agent_root, name)?;
    Ok(id)
}

/// Replace the four live memory files with the bytes of the object
/// `refs/<name>` names, then attach HEAD to `<name>`. Returns the object id.
///
/// Refuses, touching nothing, when `daemon_socket` is `Some` and exists: a
/// running daemon holds the live JSONL open and would keep appending to the
/// replaced inode. Every object file is read and hash-checked before any live
/// file is replaced. Each live file is written by copy (never hardlinked to the
/// object) to a temp sibling and renamed. The four renames are sequential; a
/// crash between them can leave a torn live store, and HEAD is written only
/// after all four succeed.
pub fn checkout(
    agent_root: &AgentRoot,
    name: &str,
    daemon_socket: Option<&Path>,
) -> Result<String, SnapshotError> {
    validate_name(name)?;
    if let Some(sock) = daemon_socket {
        if sock.exists() {
            return Err(SnapshotError::DaemonRunning(sock.to_path_buf()));
        }
    }
    let id = read_ref_id(&agent_root.branches_refs_dir(), name)?;
    let obj = agent_root.branches_objects_dir().join(&id);
    let manifest_path = obj.join(MANIFEST_FILE);
    let raw = fs::read(&manifest_path).map_err(io_err(&manifest_path))?;
    let bad = |reason: String| SnapshotError::BadObject {
        id: id.clone(),
        reason,
    };
    if sha256_hex(&raw) != id {
        return Err(bad("manifest hash does not match the object id".to_string()));
    }
    let manifest: Manifest = serde_json::from_slice(&raw)?;

    // Stage every replacement in memory before the first live rename.
    let mut plan: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::with_capacity(4);
    for (rel, live) in live_files(agent_root) {
        let bytes = match manifest.files.iter().find(|f| f.path == rel) {
            Some(entry) => {
                let src = obj.join(rel);
                let bytes = fs::read(&src).map_err(io_err(&src))?;
                if sha256_hex(&bytes) != entry.sha256 {
                    return Err(bad(format!("{rel} does not match its manifest sha256")));
                }
                Some(bytes)
            }
            None => None,
        };
        plan.push((live, bytes));
    }

    for (live, bytes) in &plan {
        match bytes {
            Some(bytes) => write_replace(live, bytes)?,
            None => match fs::remove_file(live) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(live)(e)),
            },
        }
    }
    write_head(agent_root, name)?;
    Ok(id)
}

/// Remove `refs/<name>`. The object directory stays. Refuses when HEAD is
/// attached to `<name>`; a missing ref is [`SnapshotError::BranchNotFound`].
pub fn delete_branch(agent_root: &AgentRoot, name: &str) -> Result<(), SnapshotError> {
    validate_name(name)?;
    if head_branch(agent_root)?.as_deref() == Some(name) {
        return Err(SnapshotError::BranchIsHead(name.to_string()));
    }
    let path = agent_root.branches_refs_dir().join(name);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(SnapshotError::BranchNotFound(name.to_string()))
        }
        Err(e) => Err(io_err(&path)(e)),
    }
}

/// Every well-named ref under `refs/`, sorted by name. Temp files and other
/// names outside the grammar are skipped. A missing `refs/` is an empty list.
pub fn list_branches(agent_root: &AgentRoot) -> Result<Vec<BranchRow>, SnapshotError> {
    let refs_dir = agent_root.branches_refs_dir();
    let entries = match fs::read_dir(&refs_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(&refs_dir)(e)),
    };
    let head = head_branch(agent_root)?;
    let mut rows = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err(&refs_dir))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_valid_branch_name(&name) || !entry.path().is_file() {
            continue;
        }
        let id = read_ref_id(&refs_dir, &name)?;
        let current = head.as_deref() == Some(name.as_str());
        rows.push(BranchRow { name, id, current });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // 2026-09-29T14:03:00Z
    const NOW_SEC: i64 = 1_790_690_580;

    fn scaffold(jsonl: &[u8], lessons: Option<&[u8]>) -> (tempfile::TempDir, AgentRoot) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = AgentRoot::new(dir.path());
        fs::create_dir_all(root.episodic_jsonl().parent().unwrap()).unwrap();
        fs::write(root.episodic_jsonl(), jsonl).unwrap();
        if let Some(lessons) = lessons {
            fs::create_dir_all(root.semantic_dir()).unwrap();
            fs::write(root.lessons_md(), lessons).unwrap();
        }
        (dir, root)
    }

    fn object_dir(root: &AgentRoot, id: &str) -> PathBuf {
        root.branches_objects_dir().join(id)
    }

    #[test]
    fn identical_file_sets_share_an_id_and_manifest_is_canonical() {
        let (_a, root_a) = scaffold(b"{\"x\":1}\n", Some(b"# lessons\n"));
        let (_b, root_b) = scaffold(b"{\"x\":1}\n", Some(b"# lessons\n"));

        let id_a = create_autosnap(&root_a, NOW_SEC).unwrap();
        let id_b = create_autosnap(&root_b, NOW_SEC + 60).unwrap();
        assert_eq!(id_a, id_b);
        assert_eq!(id_a.len(), 64);
        assert!(id_a
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));

        let manifest = fs::read(object_dir(&root_a, &id_a).join(MANIFEST_FILE)).unwrap();
        assert_eq!(sha256_hex(&manifest), id_a);
        let expected = format!(
            "{{\"schema_version\":\"branches/1.0\",\"files\":[\
             {{\"path\":\"episodic/AGENT_LEARNINGS.jsonl\",\"sha256\":\"{}\",\"size\":8}},\
             {{\"path\":\"semantic/LESSONS.md\",\"sha256\":\"{}\",\"size\":10}}]}}\n",
            sha256_hex(b"{\"x\":1}\n"),
            sha256_hex(b"# lessons\n"),
        );
        assert_eq!(String::from_utf8(manifest).unwrap(), expected);
    }

    #[test]
    fn manifest_bytes_sorts_by_path_byte_order() {
        let f = |p: &str| ManifestFile {
            path: p.to_string(),
            sha256: "0".repeat(64),
            size: 0,
        };
        let bytes = manifest_bytes(&[
            f("semantic/recurrence_counts.json"),
            f("personal/PREFERENCES.md"),
            f("semantic/LESSONS.md"),
            f("episodic/AGENT_LEARNINGS.jsonl"),
        ])
        .unwrap();
        let parsed: Manifest = serde_json::from_slice(&bytes).unwrap();
        let paths: Vec<_> = parsed.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "episodic/AGENT_LEARNINGS.jsonl",
                "personal/PREFERENCES.md",
                "semantic/LESSONS.md",
                "semantic/recurrence_counts.json",
            ]
        );
        assert!(bytes.ends_with(b"}\n") && !bytes.ends_with(b"\n\n"));
    }

    #[test]
    fn missing_lessons_is_omitted_not_an_error() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);

        let id = create_autosnap(&root, NOW_SEC).expect("absent LESSONS.md is fine");

        let obj = object_dir(&root, &id);
        assert!(obj.join("episodic/AGENT_LEARNINGS.jsonl").is_file());
        assert!(!obj.join("semantic/LESSONS.md").exists());
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(obj.join(MANIFEST_FILE)).unwrap()).unwrap();
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, "episodic/AGENT_LEARNINGS.jsonl");
    }

    #[test]
    fn object_file_is_a_copy_of_the_live_log() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        let id = create_autosnap(&root, NOW_SEC).unwrap();

        let mut live = fs::OpenOptions::new()
            .append(true)
            .open(root.episodic_jsonl())
            .unwrap();
        live.write_all(b"{\"x\":2}\n").unwrap();
        live.sync_all().unwrap();

        let obj_bytes =
            fs::read(object_dir(&root, &id).join("episodic/AGENT_LEARNINGS.jsonl")).unwrap();
        assert_eq!(obj_bytes, b"{\"x\":1}\n");
    }

    #[test]
    fn second_object_hardlinks_a_reused_file_from_the_first() {
        let (dir, root) = scaffold(b"{\"x\":1}\n", Some(b"# lessons\n"));
        let first = create_autosnap(&root, NOW_SEC).unwrap();

        fs::write(root.episodic_jsonl(), b"{\"x\":1}\n{\"x\":2}\n").unwrap();
        let second = create_autosnap(&root, NOW_SEC + 1).unwrap();
        assert_ne!(first, second);

        let a = object_dir(&root, &first).join("semantic/LESSONS.md");
        let b = object_dir(&root, &second).join("semantic/LESSONS.md");
        assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
        assert_eq!(
            fs::read(object_dir(&root, &second).join("episodic/AGENT_LEARNINGS.jsonl")).unwrap(),
            b"{\"x\":1}\n{\"x\":2}\n"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Only demand a shared inode where this filesystem can hardlink.
            let probe_src = dir.path().join("probe-src");
            fs::write(&probe_src, b"p").unwrap();
            if fs::hard_link(&probe_src, dir.path().join("probe-dst")).is_ok() {
                assert_eq!(
                    fs::metadata(&a).unwrap().ino(),
                    fs::metadata(&b).unwrap().ino()
                );
            }
            // The live file is never an alias of an object file.
            assert_ne!(
                fs::metadata(root.lessons_md()).unwrap().ino(),
                fs::metadata(&a).unwrap().ino()
            );
        }
    }

    #[test]
    fn ref_is_two_lines_named_for_the_timestamp() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        let id = create_autosnap(&root, NOW_SEC).unwrap();

        assert_eq!(autosnap_ref_name(NOW_SEC).unwrap(), "snap-20260929t140300z");
        let body = fs::read_to_string(root.branches_refs_dir().join("snap-20260929t140300z"))
            .expect("ref written");
        assert_eq!(body, format!("{id}\n2026-09-29T14:03:00Z\n"));

        let refs: Vec<_> = fs::read_dir(root.branches_refs_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(refs, ["snap-20260929t140300z"], "no temp file left behind");
        assert!(!root.branches_dir().join("HEAD").exists()); // never written by this module
    }

    #[test]
    fn leftover_partial_is_replaced_and_existing_object_is_kept() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        let entries: Vec<ManifestFile> = read_live(&root).into_iter().map(|(m, _)| m).collect();
        let id = sha256_hex(&manifest_bytes(&entries).unwrap());

        // A crashed write of this same id left junk behind.
        let partial = root.branches_objects_dir().join(format!("{id}.partial"));
        fs::create_dir_all(&partial).unwrap();
        fs::write(partial.join("junk"), b"x").unwrap();

        assert_eq!(create_autosnap(&root, NOW_SEC).unwrap(), id);
        let obj = object_dir(&root, &id);
        assert!(obj.join(MANIFEST_FILE).is_file());
        assert!(!obj.join("junk").exists());
        assert!(!partial.exists());

        let before = fs::metadata(obj.join(MANIFEST_FILE))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(create_autosnap(&root, NOW_SEC + 1).unwrap(), id);
        let after = fs::metadata(obj.join(MANIFEST_FILE))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "an existing object is not rewritten");
    }

    fn head_path(root: &AgentRoot) -> PathBuf {
        root.branches_dir().join(HEAD_FILE)
    }

    #[test]
    fn branch_name_grammar() {
        for ok in ["main", "a", "0x", "feat.v2", "a_b-c", &"a".repeat(64)] {
            assert!(is_valid_branch_name(ok), "{ok:?} should be valid");
        }
        for bad in [
            "",
            "Main",
            "-a",
            ".a",
            "_a",
            "a..b",
            "a/b",
            "a b",
            "HEAD",
            &"a".repeat(65),
        ] {
            assert!(!is_valid_branch_name(bad), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn create_branch_writes_ref_and_head_but_autosnap_alone_does_not() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        create_autosnap(&root, NOW_SEC).unwrap();
        assert!(!head_path(&root).exists(), "autosnap never writes HEAD");

        let id = create_branch(&root, "main", NOW_SEC + 1).unwrap();
        assert_eq!(
            fs::read_to_string(root.branches_refs_dir().join("main")).unwrap(),
            format!("{id}\n2026-09-29T14:03:01Z\n")
        );
        assert_eq!(
            fs::read_to_string(head_path(&root)).unwrap(),
            "ref: refs/main\n"
        );
        assert!(object_dir(&root, &id).join(MANIFEST_FILE).is_file());

        let rows = list_branches(&root).unwrap();
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["main", "snap-20260929t140300z", "snap-20260929t140301z"]
        );
        assert!(rows[0].current && !rows[1].current && !rows[2].current);
        assert_eq!(rows[0].id, id);
    }

    #[test]
    fn create_branch_rejects_bad_name() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        assert!(matches!(
            create_branch(&root, "../x", NOW_SEC),
            Err(SnapshotError::InvalidName(_))
        ));
        assert!(!root.branches_dir().exists());
    }

    #[test]
    fn existing_branch_is_an_error_and_ref_bytes_are_unchanged() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        create_branch(&root, "main", NOW_SEC).unwrap();
        let ref_path = root.branches_refs_dir().join("main");
        let before = fs::read(&ref_path).unwrap();

        fs::write(root.episodic_jsonl(), b"{\"x\":1}\n{\"x\":2}\n").unwrap();
        let err = create_branch(&root, "main", NOW_SEC + 60).unwrap_err();
        assert!(matches!(err, SnapshotError::BranchExists(ref n) if n == "main"));
        assert_eq!(fs::read(&ref_path).unwrap(), before);
        assert!(
            !root
                .branches_refs_dir()
                .join("snap-20260929t140400z")
                .exists(),
            "no autosnap is taken for a refused branch"
        );
    }

    #[test]
    fn checkout_replaces_live_files_with_object_copies() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        let id_a = create_branch(&root, "a", NOW_SEC).unwrap();

        // Diverge: grow the log and add lessons, then branch b.
        fs::write(root.episodic_jsonl(), b"{\"x\":1}\n{\"x\":2}\n").unwrap();
        fs::create_dir_all(root.semantic_dir()).unwrap();
        fs::write(root.lessons_md(), b"# lessons\n").unwrap();
        create_branch(&root, "b", NOW_SEC + 1).unwrap();

        assert_eq!(checkout(&root, "a", None).unwrap(), id_a);
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), b"{\"x\":1}\n");
        assert!(
            !root.lessons_md().exists(),
            "file the object omits is removed"
        );
        assert_eq!(
            fs::read_to_string(head_path(&root)).unwrap(),
            "ref: refs/a\n"
        );

        // Appending to the live log does not reach the object.
        let mut live = fs::OpenOptions::new()
            .append(true)
            .open(root.episodic_jsonl())
            .unwrap();
        live.write_all(b"{\"x\":3}\n").unwrap();
        live.sync_all().unwrap();
        let obj_log = object_dir(&root, &id_a).join("episodic/AGENT_LEARNINGS.jsonl");
        assert_eq!(fs::read(&obj_log).unwrap(), b"{\"x\":1}\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(
                fs::metadata(root.episodic_jsonl()).unwrap().ino(),
                fs::metadata(&obj_log).unwrap().ino()
            );
        }
        // No learn moves a ref.
        assert!(fs::read_to_string(root.branches_refs_dir().join("a"))
            .unwrap()
            .starts_with(&id_a));

        checkout(&root, "b", None).unwrap();
        assert_eq!(
            fs::read(root.episodic_jsonl()).unwrap(),
            b"{\"x\":1}\n{\"x\":2}\n"
        );
        assert_eq!(fs::read(root.lessons_md()).unwrap(), b"# lessons\n");
        let leftovers: Vec<_> = fs::read_dir(root.episodic_jsonl().parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn checkout_refuses_when_daemon_socket_exists() {
        let (dir, root) = scaffold(b"{\"x\":1}\n", None);
        create_branch(&root, "a", NOW_SEC).unwrap();
        fs::write(root.episodic_jsonl(), b"{\"x\":1}\n{\"x\":2}\n").unwrap();
        let sock = dir.path().join("dreamd.sock");
        fs::write(&sock, b"").unwrap();

        let err = checkout(&root, "a", Some(&sock)).unwrap_err();
        assert!(matches!(err, SnapshotError::DaemonRunning(_)));
        assert!(err.to_string().contains("dreamd watch"));
        assert_eq!(
            fs::read(root.episodic_jsonl()).unwrap(),
            b"{\"x\":1}\n{\"x\":2}\n"
        );

        // An absent socket path does not block.
        fs::remove_file(&sock).unwrap();
        checkout(&root, "a", Some(&sock)).unwrap();
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), b"{\"x\":1}\n");
    }

    #[test]
    fn checkout_rejects_a_tampered_object_before_touching_live_files() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        let id = create_branch(&root, "a", NOW_SEC).unwrap();
        fs::write(root.episodic_jsonl(), b"{\"x\":9}\n").unwrap();
        let obj_log = object_dir(&root, &id).join("episodic/AGENT_LEARNINGS.jsonl");
        fs::remove_file(&obj_log).unwrap();
        fs::write(&obj_log, b"{\"x\":666}\n").unwrap();

        assert!(matches!(
            checkout(&root, "a", None),
            Err(SnapshotError::BadObject { .. })
        ));
        assert_eq!(fs::read(root.episodic_jsonl()).unwrap(), b"{\"x\":9}\n");
    }

    #[test]
    fn checkout_of_missing_branch_is_not_found() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        assert!(matches!(
            checkout(&root, "nope", None),
            Err(SnapshotError::BranchNotFound(_))
        ));
    }

    #[test]
    fn delete_refuses_head_and_keeps_objects() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        let id_a = create_branch(&root, "a", NOW_SEC).unwrap();
        create_branch(&root, "b", NOW_SEC + 1).unwrap(); // HEAD -> b

        let err = delete_branch(&root, "b").unwrap_err();
        assert!(matches!(err, SnapshotError::BranchIsHead(ref n) if n == "b"));
        assert!(root.branches_refs_dir().join("b").is_file());

        delete_branch(&root, "a").unwrap();
        assert!(!root.branches_refs_dir().join("a").exists());
        assert!(object_dir(&root, &id_a).join(MANIFEST_FILE).is_file());

        assert!(matches!(
            delete_branch(&root, "a"),
            Err(SnapshotError::BranchNotFound(_))
        ));
    }

    #[test]
    fn list_skips_names_outside_the_grammar() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        let id = create_branch(&root, "main", NOW_SEC).unwrap();
        let refs = root.branches_refs_dir();
        fs::write(refs.join(".main.tmp"), format!("{id}\n")).unwrap();
        fs::write(refs.join("Upper"), format!("{id}\n")).unwrap();

        let names: Vec<_> = list_branches(&root)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["main", "snap-20260929t140300z"]);
    }

    #[test]
    fn list_without_refs_dir_is_empty() {
        let (_dir, root) = scaffold(b"{\"x\":1}\n", None);
        assert!(list_branches(&root).unwrap().is_empty());
    }
}
