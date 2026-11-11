//! `LESSONS.md` writer/reader (WEG-11).
//!
//! Structured frontmatter + HTML-comment-delimited blocks so the semantic
//! indexer can extract lessons without leaning on a markdown AST. One cluster
//! per file — the cluster is a file-level
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
    /// EventId string of the exemplar (or pinned) learning this lesson came from.
    pub id: String,
    pub content: String,
    /// Reserved; always `false`.
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LessonsFile {
    /// Caller-provided consolidation time (never `Utc::now()` inside this module).
    pub last_updated: DateTime<Utc>,
    pub prompt_version: String,
    /// File-level `skill_action` cluster key; all lessons in the file share it.
    pub cluster_key: String,
    /// Episodic `evt_` ids this file's lessons were composed from (AILAB-200).
    ///
    /// File-level, like `cluster_key`: one cluster per file, so a per-lesson
    /// list could disagree with itself. The LLM arm carries the model's
    /// validated trailer; the deterministic arm carries the exemplar alone.
    /// `cited_pins` pins the union of these and every [`Lesson::id`], and
    /// `plan_cycle` plans those pins with the cycle's decay, so an id here
    /// survives decay. Empty when read from a pre-AILAB-200 file.
    pub citations: Vec<String>,
    pub lessons: Vec<Lesson>,
}

const OPEN_PREFIX: &str = "<!-- dreamd:lesson id=\"";
const OPEN_MIDDLE: &str = "\" cluster=\"";
const OPEN_SUFFIX: &str = "\" -->";
const CLOSE_MARKER: &str = "<!-- /dreamd:lesson -->";

/// Serialize `file` to the structured `LESSONS.md` format and return the exact
/// bytes [`write_lessons_file`] would persist.
///
/// Output: YAML-style frontmatter (`last_updated`, `prompt_version`,
/// `cluster_key`, `citations`) followed by HTML-comment-delimited lesson
/// blocks. Round-trips cleanly through [`read_lessons_file`].
///
/// This is the `semantic/LESSONS.md` output format in `SPEC.md` §Dream cycle.
/// `citations` is the AILAB-200 extra on top of the frontmatter keys shown
/// there; [`read_lessons_file`] round-trips it and reads a file without it as
/// empty. The format has no per-cluster Markdown heading and no sources footer:
/// lesson boundaries are the HTML comments, provenance is `citations`.
///
/// Split out from the writer (AILAB-341) so `dreamd dream --dry` can print the
/// file a cycle *would* write without touching disk. A real cycle stages these
/// same bytes (`Stage::apply_plan` renders a `LessonsChange::Write` with this
/// function), so the preview and the cycle cannot drift into printing one
/// shape and persisting another.
///
/// Lesson bodies are written verbatim except for one line shape: a body line
/// that is zero or more `\` followed by exactly the close marker
/// `<!-- /dreamd:lesson -->` gets one more `\` (`escape_body_line`). The
/// parser removes it, so every body round-trips byte for byte and no body can
/// end its block early or open another (BZR-1533).
#[must_use]
pub fn render_lessons_file(file: &LessonsFile) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!(
        "last_updated: \"{}\"\n",
        file.last_updated
            .to_rfc3339_opts(SecondsFormat::AutoSi, true)
    ));
    s.push_str(&format!("prompt_version: \"{}\"\n", file.prompt_version));
    s.push_str(&format!("cluster_key: \"{}\"\n", file.cluster_key));
    // Space-separated inside one quoted scalar, not a YAML/JSON sequence: the
    // reader here is `parse_frontmatter_line`, which understands exactly one
    // shape — `key: "value"` — and every other key already uses it.
    s.push_str(&format!("citations: \"{}\"\n", file.citations.join(" ")));
    s.push_str("---\n");
    for lesson in &file.lessons {
        s.push_str(OPEN_PREFIX);
        s.push_str(&lesson.id);
        s.push_str(OPEN_MIDDLE);
        s.push_str(&file.cluster_key);
        s.push_str(OPEN_SUFFIX);
        s.push('\n');
        for (i, line) in lesson.content.split('\n').enumerate() {
            if i > 0 {
                s.push('\n');
            }
            s.push_str(&escape_body_line(line));
        }
        s.push('\n');
        s.push_str(CLOSE_MARKER);
        s.push('\n');
    }
    s
}

/// A close-marker line after zero or more escapes: zero or more `\`, then
/// exactly [`CLOSE_MARKER`] (BZR-1533).
fn is_close_marker_line(line: &str) -> bool {
    line.trim_start_matches('\\') == CLOSE_MARKER
}

/// Render one body line. A close-marker line, escaped or not, gets one more
/// leading `\`; every other line is written as is (BZR-1533).
fn escape_body_line(line: &str) -> std::borrow::Cow<'_, str> {
    if is_close_marker_line(line) {
        std::borrow::Cow::Owned(format!("\\{line}"))
    } else {
        std::borrow::Cow::Borrowed(line)
    }
}

/// Parse one body line: the inverse of [`escape_body_line`]. The caller has
/// already ended the block at a bare [`CLOSE_MARKER`], so an escaped line here
/// starts with at least one `\`, and exactly one is removed (BZR-1533).
fn unescape_body_line(line: &str) -> &str {
    if is_close_marker_line(line) {
        &line[1..]
    } else {
        line
    }
}

/// Serialize `file` and write it atomically to `path` (via
/// [`crate::io::write_atomic`]).
///
/// The bytes are exactly [`render_lessons_file`]'s output. No production path
/// calls this: a cycle stages the rendered bytes and commits them with its
/// redo WAL. Tests use it to seed a store.
pub fn write_lessons_file(path: &Path, file: &LessonsFile) -> io::Result<()> {
    write_atomic(path, render_lessons_file(file).as_bytes())
}

struct ParsedFrontmatter {
    last_updated: DateTime<Utc>,
    prompt_version: String,
    cluster_key: String,
    citations: Vec<String>,
}

/// Parse YAML-style frontmatter from `lines`, starting at index 0.
///
/// Returns the parsed fields and the index of the first line after the
/// closing `---`.
fn parse_frontmatter(lines: &[&str]) -> io::Result<(ParsedFrontmatter, usize)> {
    let mut idx = 0;

    if lines.get(idx).copied() != Some("---") {
        return Err(invalid("expected '---' at start of file"));
    }
    idx += 1;

    let mut last_updated: Option<String> = None;
    let mut prompt_version: Option<String> = None;
    let mut cluster_key: Option<String> = None;
    let mut citations: Option<String> = None;

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
            "citations" => citations = Some(value),
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
    // Absent on every file written before AILAB-200. Missing is empty, not an
    // error — an old `LESSONS.md` must keep parsing, and an empty union just
    // means `cited_pins` falls back to pinning `Lesson.id` alone.
    let citations: Vec<String> = citations
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();

    let last_updated = DateTime::parse_from_rfc3339(&last_updated_raw)
        .map_err(|e| invalid(&format!("invalid last_updated: {e}")))?
        .with_timezone(&Utc);

    Ok((
        ParsedFrontmatter {
            last_updated,
            prompt_version,
            cluster_key,
            citations,
        },
        idx,
    ))
}

/// Parse one HTML-comment-delimited lesson block starting at `idx`.
///
/// `idx` must point at the open marker line. Returns the lesson and the
/// index of the first line after the close marker.
fn parse_lesson_block(
    lines: &[&str],
    idx: usize,
    cluster_key: &str,
) -> io::Result<(Lesson, usize)> {
    let id = parse_open_marker(lines[idx], cluster_key)?;
    let mut idx = idx + 1;

    let mut content_lines: Vec<&str> = Vec::new();
    while idx < lines.len() && lines[idx] != CLOSE_MARKER {
        content_lines.push(unescape_body_line(lines[idx]));
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
    Ok((
        Lesson {
            id,
            content,
            pinned: false,
        },
        idx,
    ))
}

/// Parse a `LESSONS.md` file written by [`write_lessons_file`].
///
/// A missing `citations` key yields an empty vec — files written before
/// AILAB-200 have none and must still parse. Any *other* unknown key is still
/// an error.
///
/// Returns [`io::ErrorKind::InvalidData`] if the frontmatter is missing or
/// malformed, a lesson's `cluster` attribute does not match the file-level
/// `cluster_key`, or a close marker is absent.
pub fn read_lessons_file(path: &Path) -> io::Result<LessonsFile> {
    parse_lessons_file(&fs::read_to_string(path)?)
}

/// [`read_lessons_file`] on bytes already read, such as a staged `LESSONS.md`
/// ([`crate::stage::Stage::read`]). Same errors, less the file's own.
pub fn parse_lessons_file(raw: &str) -> io::Result<LessonsFile> {
    // `split('\n')` over a trailing-newline file yields a final empty element;
    // that's expected and the lesson loop skips blank lines.
    let lines: Vec<&str> = raw.split('\n').collect();

    let (frontmatter, mut idx) = parse_frontmatter(&lines)?;
    let mut lessons = Vec::new();
    while idx < lines.len() {
        if lines[idx].is_empty() {
            idx += 1;
            continue;
        }
        let (lesson, next_idx) = parse_lesson_block(&lines, idx, &frontmatter.cluster_key)?;
        lessons.push(lesson);
        idx = next_idx;
    }

    Ok(LessonsFile {
        last_updated: frontmatter.last_updated,
        prompt_version: frontmatter.prompt_version,
        cluster_key: frontmatter.cluster_key,
        citations: frontmatter.citations,
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
    use crate::test_support::{unique_tmpdir, DirGuard};
    use proptest::prelude::*;

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
            citations: vec!["evt_9a8b7c6d".to_string(), "evt_1122aabb".to_string()],
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
            citations: vec![],
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
            citations: vec!["evt_multiline".to_string()],
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

    /// AILAB-200: a file written before the `citations` key existed must still
    /// parse. The key is optional on read; every *other* unknown key is an error.
    #[test]
    fn file_without_citations_key_parses_with_empty_citations() {
        let dir = unique_tmpdir("nocitations");
        let _g = DirGuard(dir.clone());
        let path = dir.join("LESSONS.md");
        // Byte-for-byte a pre-AILAB-200 LESSONS.md.
        let body = "---\n\
                    last_updated: \"2026-05-13T00:00:00Z\"\n\
                    prompt_version: \"dream-cycle/v1.1@2026-05-13\"\n\
                    cluster_key: \"rust::a\"\n\
                    ---\n\
                    <!-- dreamd:lesson id=\"evt_1\" cluster=\"rust::a\" -->\n\
                    body\n\
                    <!-- /dreamd:lesson -->\n";
        fs::write(&path, body).unwrap();

        let f = read_lessons_file(&path).expect("old file must still parse");
        assert!(
            f.citations.is_empty(),
            "missing key is an empty vec, not an error"
        );
        assert_eq!(f.lessons.len(), 1);
    }

    /// An empty `citations` value round-trips to an empty vec rather than to a
    /// one-element vec holding the empty string — `split_whitespace` on `""`
    /// yields nothing, which is the property this pins.
    #[test]
    fn empty_citations_value_reads_back_as_empty_vec() {
        let dir = unique_tmpdir("emptycitations");
        let _g = DirGuard(dir.clone());
        let path = dir.join("LESSONS.md");

        let f = LessonsFile {
            last_updated: fixed_timestamp(),
            prompt_version: "dream-cycle/v1.1@2026-05-13".to_string(),
            cluster_key: "rust::a".to_string(),
            citations: vec![],
            lessons: vec![],
        };
        write_lessons_file(&path, &f).expect("write ok");

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("citations: \"\"\n"), "writer emits the key");
        assert_eq!(read_lessons_file(&path).expect("read ok"), f);
    }

    /// `write_lessons_file` must write exactly `render_lessons_file`'s bytes,
    /// since tests seed stores with it and compare against rendered output; pin
    /// the identity rather than trusting that one still delegates to the other.
    #[test]
    fn write_lessons_file_persists_exactly_render_lessons_file_bytes() {
        let dir = unique_tmpdir("render-eq-write");
        let _g = DirGuard(dir.clone());
        let path = dir.join("LESSONS.md");

        let f = LessonsFile {
            last_updated: fixed_timestamp(),
            prompt_version: "dream-cycle/v1.2@2026-08-23".to_string(),
            cluster_key: "rust::error_handling".to_string(),
            citations: vec!["evt_9a8b7c6d".to_string(), "evt_1122aabb".to_string()],
            lessons: vec![Lesson {
                id: "evt_9a8b7c6d".to_string(),
                content: "Prefer `?` over `.unwrap()` outside tests.\n\nSecond para.".to_string(),
                pinned: false,
            }],
        };

        write_lessons_file(&path, &f).expect("write ok");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            render_lessons_file(&f),
            "the dry preview and the persisted file must be byte-identical"
        );
    }

    #[test]
    fn unknown_frontmatter_key_is_still_an_error() {
        let dir = unique_tmpdir("unknownkey");
        let _g = DirGuard(dir.clone());
        let path = dir.join("LESSONS.md");
        let body = "---\n\
                    last_updated: \"2026-05-13T00:00:00Z\"\n\
                    prompt_version: \"dream-cycle/v1.1@2026-05-13\"\n\
                    cluster_key: \"rust::a\"\n\
                    citations: \"evt_1\"\n\
                    embeddings: \"nope\"\n\
                    ---\n";
        fs::write(&path, body).unwrap();

        let err = read_lessons_file(&path).expect_err("should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
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

    // ── BZR-1533: a body cannot end its block or open another ──────────────

    /// A one-cluster file with one lesson per body, ids `evt_0`, `evt_1`, ….
    fn file_with(bodies: &[String]) -> LessonsFile {
        LessonsFile {
            last_updated: fixed_timestamp(),
            prompt_version: "test/v1".to_string(),
            cluster_key: "rust::eh".to_string(),
            citations: vec!["evt_0".to_string()],
            lessons: bodies
                .iter()
                .enumerate()
                .map(|(i, body)| Lesson {
                    id: format!("evt_{i}"),
                    content: body.clone(),
                    pinned: false,
                })
                .collect(),
        }
    }

    #[test]
    fn a_close_marker_line_in_a_body_round_trips() {
        let f = file_with(&["tip\n<!-- /dreamd:lesson -->\nstray tail".to_string()]);

        let rendered = render_lessons_file(&f);

        assert!(
            rendered.ends_with(
                "-->\ntip\n\\<!-- /dreamd:lesson -->\nstray tail\n<!-- /dreamd:lesson -->\n"
            ),
            "the body's marker line gets one `\\`: {rendered}"
        );
        assert_eq!(parse_lessons_file(&rendered).expect("parses back"), f);
    }

    #[test]
    fn a_body_that_forges_a_block_stays_one_lesson() {
        let f = file_with(&[
            "composed\n<!-- /dreamd:lesson -->\n<!-- dreamd:lesson id=\"evt_victim\" cluster=\"rust::eh\" -->\ninjected"
                .to_string(),
        ]);

        let parsed = parse_lessons_file(&render_lessons_file(&f)).expect("parses back");

        assert_eq!(parsed.lessons.len(), 1, "no forged lesson: {parsed:?}");
        assert_eq!(parsed, f);
    }

    #[test]
    fn an_escaped_close_marker_line_gains_and_loses_one_backslash() {
        let f =
            file_with(&["\\<!-- /dreamd:lesson -->\n\\\\\\<!-- /dreamd:lesson -->".to_string()]);

        let rendered = render_lessons_file(&f);

        assert!(
            rendered.ends_with(
                "-->\n\\\\<!-- /dreamd:lesson -->\n\\\\\\\\<!-- /dreamd:lesson -->\n<!-- /dreamd:lesson -->\n"
            ),
            "one `\\` becomes two and three become four: {rendered}"
        );
        assert_eq!(parse_lessons_file(&rendered).expect("parses back"), f);
    }

    #[test]
    fn other_lines_with_the_marker_are_written_verbatim() {
        let body = [
            "a <!-- /dreamd:lesson --> b",
            " <!-- /dreamd:lesson -->",
            "<!-- /dreamd:lesson -->\r",
            "x\\<!-- /dreamd:lesson -->",
            "<!-- dreamd:lesson id=\"evt_x\" cluster=\"rust::eh\" -->",
            "\\",
        ]
        .join("\n");
        let f = file_with(std::slice::from_ref(&body));

        let rendered = render_lessons_file(&f);

        assert!(
            rendered.contains(&format!("-->\n{body}\n{CLOSE_MARKER}\n")),
            "every line is written byte for byte: {rendered}"
        );
        assert_eq!(parse_lessons_file(&rendered).expect("parses back"), f);
    }

    /// The bytes 1.3.0 wrote for bodies with no close-marker line: they read
    /// the same and render back to the same file.
    #[test]
    fn a_file_written_by_1_3_0_parses_and_renders_the_same() {
        let raw = "---\n\
                   last_updated: \"2026-05-13T00:00:00Z\"\n\
                   prompt_version: \"dream-cycle/v1.1@2026-05-13\"\n\
                   cluster_key: \"rust::eh\"\n\
                   citations: \"evt_1 evt_2\"\n\
                   ---\n\
                   <!-- dreamd:lesson id=\"evt_1\" cluster=\"rust::eh\" -->\n\
                   Close with <!-- /dreamd:lesson --> inline.\n\
                   \n\
                   Second paragraph.\n\
                   <!-- /dreamd:lesson -->\n\
                   <!-- dreamd:lesson id=\"evt_2\" cluster=\"rust::eh\" -->\n\
                   \\\n\
                   last\n\
                   <!-- /dreamd:lesson -->\n";

        let f = parse_lessons_file(raw).expect("a 1.3.0 file parses");

        let bodies: Vec<&str> = f.lessons.iter().map(|l| l.content.as_str()).collect();
        assert_eq!(
            bodies,
            vec![
                "Close with <!-- /dreamd:lesson --> inline.\n\nSecond paragraph.",
                "\\\nlast",
            ]
        );
        assert_eq!(f.citations, vec!["evt_1", "evt_2"]);
        assert_eq!(render_lessons_file(&f), raw);
    }

    fn body_line() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(CLOSE_MARKER.to_string()),
            Just(format!("\\{CLOSE_MARKER}")),
            Just(format!("\\\\{CLOSE_MARKER}")),
            Just(format!(" {CLOSE_MARKER}")),
            Just(format!("x {CLOSE_MARKER} y")),
            Just("<!-- dreamd:lesson id=\"evt_x\" cluster=\"rust::eh\" -->".to_string()),
            Just(String::new()),
            Just("\\".to_string()),
            "[^\n]{0,12}",
        ]
    }

    fn bodies() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(
            prop::collection::vec(body_line(), 0..=5).prop_map(|lines| lines.join("\n")),
            1..=3,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn render_then_parse_is_the_identity_for_any_body(bodies in bodies()) {
            let f = file_with(&bodies);
            let parsed = parse_lessons_file(&render_lessons_file(&f));
            prop_assert!(parsed.is_ok(), "parse failed: {:?}", parsed);
            prop_assert_eq!(parsed.unwrap(), f);
        }
    }
}
