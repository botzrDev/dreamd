//! `LESSONS.md` writer/reader (DR-109 / WEG-11).
//!
//! Structured frontmatter + HTML-comment-delimited blocks so the v0.1.1
//! semantic indexer (DR-211) can extract lessons without leaning on a
//! markdown AST. One cluster per file — the cluster is a file-level
//! invariant carried on [`LessonsFile::cluster_key`], not a per-lesson
//! field, so an inconsistent state is unrepresentable by construction.
//!
//! `last_updated` is caller-provided: the dream-cycle pipeline owns the
//! "when did this lesson get consolidated" decision; this module is
//! plumbing. Tests rely on this — never call `Utc::now()` here.

use std::fs;
use std::io;
use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};

use crate::io::write_atomic;

#[derive(Debug, Clone, PartialEq)]
pub struct Lesson {
    pub id: String,
    pub content: String,
    /// Reserved; always `false` in v0.1. Forward-compat slot for v0.2.
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LessonsFile {
    pub last_updated: DateTime<Utc>,
    pub prompt_version: String,
    pub cluster_key: String,
    pub lessons: Vec<Lesson>,
}

const OPEN_PREFIX: &str = "<!-- dreamd:lesson id=\"";
const OPEN_MIDDLE: &str = "\" cluster=\"";
const OPEN_SUFFIX: &str = "\" -->";
const CLOSE_MARKER: &str = "<!-- /dreamd:lesson -->";

pub fn write_lessons_file(path: &Path, file: &LessonsFile) -> io::Result<()> {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!(
        "last_updated: \"{}\"\n",
        file.last_updated
            .to_rfc3339_opts(SecondsFormat::AutoSi, true)
    ));
    s.push_str(&format!("prompt_version: \"{}\"\n", file.prompt_version));
    s.push_str(&format!("cluster_key: \"{}\"\n", file.cluster_key));
    s.push_str("---\n");
    for lesson in &file.lessons {
        s.push_str(OPEN_PREFIX);
        s.push_str(&lesson.id);
        s.push_str(OPEN_MIDDLE);
        s.push_str(&file.cluster_key);
        s.push_str(OPEN_SUFFIX);
        s.push('\n');
        s.push_str(&lesson.content);
        s.push('\n');
        s.push_str(CLOSE_MARKER);
        s.push('\n');
    }
    write_atomic(path, s.as_bytes())
}

pub fn read_lessons_file(path: &Path) -> io::Result<LessonsFile> {
    let raw = fs::read_to_string(path)?;
    // `split('\n')` over a trailing-newline file yields a final empty element;
    // that's expected and the lesson loop skips blank lines.
    let lines: Vec<&str> = raw.split('\n').collect();
    let mut idx = 0;

    if lines.get(idx).copied() != Some("---") {
        return Err(invalid("expected '---' at start of file"));
    }
    idx += 1;

    let mut last_updated: Option<String> = None;
    let mut prompt_version: Option<String> = None;
    let mut cluster_key: Option<String> = None;

    while idx < lines.len() && lines[idx] != "---" {
        let line = lines[idx];
        idx += 1;
        if line.is_empty() {
            continue;
        }
        let (key, value) = parse_frontmatter_line(line)?;
        match key {
            "last_updated" => last_updated = Some(value),
            "prompt_version" => prompt_version = Some(value),
            "cluster_key" => cluster_key = Some(value),
            other => {
                return Err(invalid(&format!("unknown frontmatter key: {other}")));
            }
        }
    }
    if idx >= lines.len() {
        return Err(invalid("frontmatter closing '---' not found"));
    }
    idx += 1; // past closing ---

    let last_updated_raw = last_updated.ok_or_else(|| invalid("missing last_updated"))?;
    let prompt_version = prompt_version.ok_or_else(|| invalid("missing prompt_version"))?;
    let cluster_key = cluster_key.ok_or_else(|| invalid("missing cluster_key"))?;

    let last_updated = DateTime::parse_from_rfc3339(&last_updated_raw)
        .map_err(|e| invalid(&format!("invalid last_updated: {e}")))?
        .with_timezone(&Utc);

    let mut lessons = Vec::new();
    while idx < lines.len() {
        if lines[idx].is_empty() {
            idx += 1;
            continue;
        }
        let id = parse_open_marker(lines[idx], &cluster_key)?;
        idx += 1;

        let mut content_lines: Vec<&str> = Vec::new();
        while idx < lines.len() && lines[idx] != CLOSE_MARKER {
            content_lines.push(lines[idx]);
            idx += 1;
        }
        if idx >= lines.len() {
            return Err(invalid("lesson close marker not found"));
        }
        idx += 1; // past close marker

        // The writer emits `content + '\n' + CLOSE_MARKER`; that trailing '\n'
        // is consumed as the line separator between content's last line and
        // the close marker, so `content_lines` already represents the exact
        // content bytes once joined.
        let content = content_lines.join("\n");
        lessons.push(Lesson {
            id,
            content,
            pinned: false,
        });
    }

    Ok(LessonsFile {
        last_updated,
        prompt_version,
        cluster_key,
        lessons,
    })
}

fn parse_frontmatter_line(line: &str) -> io::Result<(&str, String)> {
    let (key, rest) = line
        .split_once(": ")
        .ok_or_else(|| invalid(&format!("bad frontmatter line: {line}")))?;
    if !(rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2) {
        return Err(invalid(&format!(
            "frontmatter value must be double-quoted: {line}"
        )));
    }
    Ok((key, rest[1..rest.len() - 1].to_string()))
}

fn parse_open_marker(line: &str, expected_cluster: &str) -> io::Result<String> {
    if !line.starts_with(OPEN_PREFIX) || !line.ends_with(OPEN_SUFFIX) {
        return Err(invalid(&format!("bad lesson open marker: {line}")));
    }
    let inner = &line[OPEN_PREFIX.len()..line.len() - OPEN_SUFFIX.len()];
    let (id, cluster) = inner
        .split_once(OPEN_MIDDLE)
        .ok_or_else(|| invalid(&format!("bad lesson open marker: {line}")))?;
    if cluster != expected_cluster {
        return Err(invalid(&format!(
            "lesson cluster '{cluster}' != file cluster_key '{expected_cluster}'"
        )));
    }
    Ok(id.to_string())
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_tmpdir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dreamd-lessons-{}-{}-{}-{}",
            label,
            std::process::id(),
            nanos,
            n,
        ));
        fs::create_dir_all(&dir).expect("create unique tmpdir");
        dir
    }

    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixed_timestamp() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-13T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn round_trip_multi_lesson() {
        let dir = unique_tmpdir("multi");
        let _g = DirGuard(dir.clone());
        let path = dir.join("LESSONS.md");

        let f = LessonsFile {
            last_updated: fixed_timestamp(),
            prompt_version: "dream-cycle/v1.1@2026-05-13".to_string(),
            cluster_key: "rust::error_handling".to_string(),
            lessons: vec![
                Lesson {
                    id: "evt_9a8b7c6d".to_string(),
                    content: "Prefer `?` over `.unwrap()` outside tests.".to_string(),
                    pinned: false,
                },
                Lesson {
                    id: "evt_1122aabb".to_string(),
                    content: "Wrap I/O errors in `io::Error::new(InvalidData, ...)`.".to_string(),
                    pinned: false,
                },
            ],
        };

        write_lessons_file(&path, &f).expect("write ok");
        let g = read_lessons_file(&path).expect("read ok");
        assert_eq!(f, g);
    }

    #[test]
    fn round_trip_empty_lessons() {
        let dir = unique_tmpdir("empty");
        let _g = DirGuard(dir.clone());
        let path = dir.join("LESSONS.md");

        let f = LessonsFile {
            last_updated: fixed_timestamp(),
            prompt_version: "dream-cycle/v1.1@2026-05-13".to_string(),
            cluster_key: "rust::error_handling".to_string(),
            lessons: vec![],
        };

        write_lessons_file(&path, &f).expect("write ok");
        let g = read_lessons_file(&path).expect("read ok");
        assert_eq!(f, g);
    }

    #[test]
    fn round_trip_multiline_content() {
        let dir = unique_tmpdir("multiline");
        let _g = DirGuard(dir.clone());
        let path = dir.join("LESSONS.md");

        let f = LessonsFile {
            last_updated: fixed_timestamp(),
            prompt_version: "dream-cycle/v1.1@2026-05-13".to_string(),
            cluster_key: "rust::testing".to_string(),
            lessons: vec![Lesson {
                id: "evt_multiline".to_string(),
                content: "First paragraph.\n\nSecond paragraph with code:\n\n    let x = 1;"
                    .to_string(),
                pinned: false,
            }],
        };

        write_lessons_file(&path, &f).expect("write ok");
        let g = read_lessons_file(&path).expect("read ok");
        assert_eq!(f, g);
    }

    #[test]
    fn missing_frontmatter_open_returns_invalid_data() {
        let dir = unique_tmpdir("badopen");
        let _g = DirGuard(dir.clone());
        let path = dir.join("LESSONS.md");
        fs::write(&path, b"not frontmatter\n").unwrap();

        let err = read_lessons_file(&path).expect_err("should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn cluster_mismatch_returns_invalid_data() {
        let dir = unique_tmpdir("clustermismatch");
        let _g = DirGuard(dir.clone());
        let path = dir.join("LESSONS.md");
        let body = "---\n\
                    last_updated: \"2026-05-13T00:00:00Z\"\n\
                    prompt_version: \"dream-cycle/v1.1@2026-05-13\"\n\
                    cluster_key: \"rust::a\"\n\
                    ---\n\
                    <!-- dreamd:lesson id=\"evt_1\" cluster=\"rust::b\" -->\n\
                    body\n\
                    <!-- /dreamd:lesson -->\n";
        fs::write(&path, body).unwrap();

        let err = read_lessons_file(&path).expect_err("should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
