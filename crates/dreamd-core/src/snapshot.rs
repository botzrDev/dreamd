//! Content-addressed memory objects and autosnap refs (BZR-160).
//!
//! Format is `docs/branching.md` (`branches/1.0`, BZR-159). An object is
//! `.agent/.dreamd/branches/objects/<id>/`, where `<id>` is the lowercase-hex
//! SHA-256 of the canonical manifest bytes, trailing newline included. The
//! manifest carries no id and no timestamp; when an object was taken is
//! recorded on the ref that names it, `branches/refs/snap-<yyyymmddthhmmssz>`.
//!
//! The decay archive directory is a different tree and is never written here.
//! Neither is the branch pointer file, checkout, or any CLI: those are BZR-150.
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
}
