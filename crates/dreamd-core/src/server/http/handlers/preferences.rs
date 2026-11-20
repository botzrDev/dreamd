//! `GET /api/v1/preferences` — read `.agent/personal/PREFERENCES.md`.

use std::borrow::Cow;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime};

use axum::extract::{Extension, State};
use axum::response::IntoResponse;

use crate::registry::ProjectEntry;

use super::super::router::error_500;
use super::super::state::AppState;

/// Maximum bytes served from `PREFERENCES.md` in a single response.
/// Responses exceeding this cap are truncated and annotated with
/// `X-Dreamd-Truncated: true` and `X-Dreamd-Original-Size: <n>`.
///
/// The dream-cycle prompt has the same ceiling on the same `personal/` bytes
/// (AILAB-199), so this re-exports that one definition rather than keeping a
/// second copy.
pub(crate) use crate::llm::PERSONAL_LAYER_MAX_BYTES as PREFERENCES_SIZE_CAP;

/// How long `GET /preferences` waits for its read (DRM-120).
const PREFERENCES_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The path the messages name, relative to the project.
const PREFERENCES_REL: &str = ".agent/personal/PREFERENCES.md";

/// What the route serves.
#[derive(Debug)]
struct Served {
    body: String,
    truncated: bool,
    lossy: bool,
    original_size: u64,
    modified: Option<SystemTime>,
}

/// Why the blocking read did not give an answer in time.
#[derive(Debug, PartialEq, Eq)]
enum ReadFailed {
    /// The deadline passed first.
    TimedOut,
    /// The blocking task panicked or was cancelled.
    Join,
}

/// Why the file is not served (each is a `500`).
#[derive(Debug)]
enum Refused {
    /// The path resolves outside `.agent/personal/` (D1).
    Outside,
    /// The open file is not a regular file: a FIFO, a device, a folder.
    NotRegular,
    /// The open, the `fstat` or the read failed.
    Io(std::io::Error),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Outside => write!(
                f,
                "preferences read failed: {PREFERENCES_REL} resolves outside .agent/personal/, so it is not served"
            ),
            Refused::NotRegular => write!(
                f,
                "preferences read failed: {PREFERENCES_REL} is not a regular file"
            ),
            Refused::Io(e) => write!(f, "preferences read failed: {e}"),
        }
    }
}

/// Run `read` on the blocking pool, and give up after `deadline`.
///
/// A timed-out read keeps its blocking-pool thread until it returns; only the
/// answer to the request is bounded.
async fn read_with_deadline<T: Send + 'static>(
    deadline: Duration,
    read: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ReadFailed> {
    match tokio::time::timeout(deadline, tokio::task::spawn_blocking(read)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_join)) => Err(ReadFailed::Join),
        Err(_elapsed) => Err(ReadFailed::TimedOut),
    }
}

/// Read at most `cap + 1` bytes.
fn read_capped(reader: impl Read, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    reader.take(cap as u64 + 1).read_to_end(&mut raw)?;
    Ok(raw)
}

/// Turn the bytes read into the body: valid UTF-8 of at most `cap` bytes,
/// cut at a character boundary. Returns `(body, truncated, lossy)`.
fn body_from(raw: &[u8], cap: usize) -> (String, bool, bool) {
    let mut truncated = raw.len() > cap;
    let mut kept = &raw[..raw.len().min(cap)];
    if truncated {
        kept = without_split_tail(kept);
    }
    let decoded = String::from_utf8_lossy(kept);
    let lossy = matches!(decoded, Cow::Owned(_));
    let mut body = decoded.into_owned();
    if body.len() > cap {
        // Each replacement character is 3 bytes, so invalid bytes can decode
        // past the cap.
        let mut end = cap;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        truncated = true;
    }
    (body, truncated, lossy)
}

/// `bytes` without a trailing incomplete UTF-8 sequence: the first one to
/// three bytes of a multi-byte character that a cut split.
fn without_split_tail(bytes: &[u8]) -> &[u8] {
    for back in 1..=bytes.len().min(3) {
        let start = bytes.len() - back;
        let byte = bytes[start];
        if byte & 0b1100_0000 == 0b1000_0000 {
            // A continuation byte: the character starts further back.
            continue;
        }
        let incomplete = matches!(
            std::str::from_utf8(&bytes[start..]),
            Err(e) if e.valid_up_to() == 0 && e.error_len().is_none()
        );
        return if incomplete { &bytes[..start] } else { bytes };
    }
    bytes
}

/// The blocking read (DRM-120). `Ok(None)` is a file that is not there,
/// which is served as an empty body.
fn read_preferences(personal: &Path, preferences: &Path) -> Result<Option<Served>, Refused> {
    let personal = match std::fs::canonicalize(personal) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Refused::Io(e)),
    };
    let resolved = match std::fs::canonicalize(preferences) {
        Ok(p) => p,
        // Also a symlink whose target is gone.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Refused::Io(e)),
    };
    if !resolved.starts_with(&personal) {
        return Err(Refused::Outside);
    }

    let file = open_no_wait(&resolved).map_err(Refused::Io)?;
    // `fstat` on the open file, so the check is about what was opened.
    let meta = file.metadata().map_err(Refused::Io)?;
    if !meta.is_file() {
        return Err(Refused::NotRegular);
    }
    let raw = read_capped(&file, PREFERENCES_SIZE_CAP).map_err(Refused::Io)?;
    let (body, truncated, lossy) = body_from(&raw, PREFERENCES_SIZE_CAP);
    Ok(Some(Served {
        body,
        truncated,
        lossy,
        original_size: meta.len(),
        modified: meta.modified().ok(),
    }))
}

/// Open `path` to read. On Unix with `O_NONBLOCK`, so a FIFO cannot block the
/// open, and `O_NOFOLLOW`, so a swap to a symlink after the containment check
/// fails it.
fn open_no_wait(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// `GET /api/v1/preferences` — read `.agent/personal/PREFERENCES.md`.
///
/// # Headers
/// * `X-Agent-Root` (required)
///
/// # Response (`200`)
/// `{"body":"<markdown>","last_modified":"<rfc3339>|null"}`
///
/// The file is served only when it resolves to a regular file inside
/// `.agent/personal/` (DRM-120). It is read off the async workers, at most
/// [`PREFERENCES_SIZE_CAP`] + 1 bytes, with a 5 s deadline. The body is UTF-8
/// cut at a character boundary; see `X-Dreamd-Truncated`,
/// `X-Dreamd-Original-Size` and `X-Dreamd-Lossy`.
pub(crate) async fn get_preferences(
    State(_state): State<AppState>,
    Extension(entry): Extension<ProjectEntry>,
) -> axum::response::Response {
    let root = crate::layout::AgentRoot::new(&entry.root);
    let personal = root.personal_dir();
    let pref_path = root.preferences_md();

    let read = {
        let pref_path = pref_path.clone();
        move || read_preferences(&personal, &pref_path)
    };
    let served = match read_with_deadline(PREFERENCES_READ_TIMEOUT, read).await {
        Ok(Ok(Some(served))) => served,
        Ok(Ok(None)) => {
            let body = serde_json::json!({ "body": "", "last_modified": null });
            return (axum::http::StatusCode::OK, axum::Json(body)).into_response();
        }
        Ok(Err(refused)) => return error_500(&refused.to_string()),
        Err(ReadFailed::TimedOut) => {
            return error_500(&format!(
                "preferences read timed out after {} s",
                PREFERENCES_READ_TIMEOUT.as_secs()
            ))
        }
        Err(ReadFailed::Join) => return error_500("preferences read failed: the read task failed"),
    };

    let last_modified: Option<String> = served.modified.map(|t| {
        let dt: chrono::DateTime<chrono::Utc> = t.into();
        dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
    });
    let body = serde_json::json!({
        "body": served.body,
        "last_modified": last_modified,
    });

    let mut headers = axum::http::HeaderMap::new();
    if served.truncated {
        headers.insert(
            axum::http::HeaderName::from_static("x-dreamd-truncated"),
            axum::http::HeaderValue::from_static("true"),
        );
        headers.insert(
            axum::http::HeaderName::from_static("x-dreamd-original-size"),
            axum::http::HeaderValue::from(served.original_size),
        );
        tracing::warn!(
            original_size = served.original_size,
            cap = PREFERENCES_SIZE_CAP,
            path = %pref_path.display(),
            "PREFERENCES.md truncated to 16 KB cap"
        );
    }
    if served.lossy {
        headers.insert(
            axum::http::HeaderName::from_static("x-dreamd-lossy"),
            axum::http::HeaderValue::from_static("true"),
        );
    }

    (axum::http::StatusCode::OK, headers, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// DRM-120 test 1: a cut at the cap drops a split trailing character
    /// instead of replacing it.
    #[test]
    fn a_cut_never_splits_a_character() {
        assert_eq!(PREFERENCES_SIZE_CAP, 16_384);

        let mut three = vec![b'x'; 16_383];
        three.extend_from_slice("€".as_bytes());
        let (body, truncated, lossy) = body_from(&three, PREFERENCES_SIZE_CAP);
        assert_eq!(body.len(), 16_383);
        assert!(body.bytes().all(|b| b == b'x'));
        assert!(truncated, "a 3-byte character split at the cap");
        assert!(!lossy, "the split character was replaced, not dropped");

        let mut four = vec![b'x'; 16_382];
        four.extend_from_slice("😀".as_bytes());
        let (body, truncated, lossy) = body_from(&four, PREFERENCES_SIZE_CAP);
        assert_eq!(body.len(), 16_382);
        assert!(truncated, "a 4-byte character split at the cap");
        assert!(!lossy, "the split character was replaced, not dropped");
    }

    /// DRM-120 test 2: bytes that are not UTF-8 are replaced and flagged, and
    /// the replacements never take the body past the cap.
    #[test]
    fn invalid_bytes_are_replaced_flagged_and_bounded() {
        let (body, truncated, lossy) = body_from(b"ok \xff\xfe end", PREFERENCES_SIZE_CAP);
        assert_eq!(body, "ok \u{FFFD}\u{FFFD} end");
        assert!(lossy);
        assert!(!truncated);

        let raw = vec![0xff; PREFERENCES_SIZE_CAP];
        let (body, truncated, lossy) = body_from(&raw, PREFERENCES_SIZE_CAP);
        assert!(body.len() <= PREFERENCES_SIZE_CAP, "{} bytes", body.len());
        assert!(!body.is_empty());
        assert!(body.chars().all(|c| c == '\u{FFFD}'), "a split U+FFFD");
        assert!(truncated, "the decoded body was cut");
        assert!(lossy);
    }

    /// DRM-120 test 3: the deadline answers while the read still runs.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_deadline_answers_a_read_that_hangs() {
        let started = Instant::now();
        let got = read_with_deadline(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(2));
        })
        .await;
        let took = started.elapsed();
        assert_eq!(got, Err(ReadFailed::TimedOut));
        assert!(took < Duration::from_secs(1), "took {took:?}");
    }

    /// DRM-120 test 4: the read stops one byte past the cap.
    #[test]
    fn the_read_is_capped() {
        let raw = read_capped(std::io::repeat(b'x').take(10 << 20), 16).unwrap();
        assert_eq!(raw.len(), 17);
    }
}
