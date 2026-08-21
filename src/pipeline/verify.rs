//! Media file verification: extension allowlist, sample detection, minimum
//! size, and optional ffprobe inspection.
//!
//! Fixes over the Go original (`internal/verify`):
//! - ffprobe arguments guard option-like filenames: a media path starting with
//!   `-` would be parsed as an ffprobe flag; such paths are prefixed with
//!   `./`.
//! - ffprobe is never invoked in dry-run (enforced by callers; [`probe_args`]
//!   stays pure so tests can pin the argv).

use std::path::Path;

use serde::Serialize;

/// Files smaller than this are treated as samples/broken downloads.
pub const MIN_FILE_SIZE_BYTES: i64 = 25 * 1024 * 1024;

const MEDIA_EXTENSIONS: &[&str] = &[
    ".mkv", ".mp4", ".m4v", ".avi", ".mov", ".mpg", ".mpeg", ".ts",
];

pub fn media_extension_allowed(ext: &str) -> bool {
    let ext = ext.to_ascii_lowercase();
    MEDIA_EXTENSIONS.contains(&ext.as_str())
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyResult {
    pub ok: bool,
    pub reason: String,
    pub file_path: String,
    pub file_size: i64,
}

/// Verifies a single media file. Returns `Err` only for unusable inputs or IO
/// failures; policy failures come back as `Ok(VerifyResult { ok: false, .. })`.
pub fn verify_file(path: &str) -> std::io::Result<VerifyResult> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path_required",
        ));
    }
    let cleaned = Path::new(trimmed);
    let meta = std::fs::metadata(cleaned)?;
    if meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not_a_file",
        ));
    }
    let size = meta.len() as i64;
    let path_str = cleaned.to_string_lossy().into_owned();

    let ext = cleaned
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    if !media_extension_allowed(&ext) {
        return Ok(VerifyResult {
            ok: false,
            reason: "extension_not_allowed".into(),
            file_path: path_str,
            file_size: size,
        });
    }

    let name = cleaned
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("sample") || name.contains("trailer") {
        return Ok(VerifyResult {
            ok: false,
            reason: "sample_detected".into(),
            file_path: path_str,
            file_size: size,
        });
    }

    if size < MIN_FILE_SIZE_BYTES {
        return Ok(VerifyResult {
            ok: false,
            reason: "file_too_small".into(),
            file_path: path_str,
            file_size: size,
        });
    }

    Ok(VerifyResult {
        ok: true,
        reason: "ok".into(),
        file_path: path_str,
        file_size: size,
    })
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FFProbeSummary {
    pub duration_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
}

/// Builds the ffprobe argv for a media path. A filename that begins with `-`
/// would otherwise be consumed as an option by ffprobe; prefixing with `./`
/// keeps it a positional operand.
pub fn probe_args(media_path: &str) -> Vec<String> {
    let mut args = vec![
        "-v".to_string(),
        "error".to_string(),
        "-select_streams".to_string(),
        "v:0".to_string(),
        "-show_entries".to_string(),
        "format=duration:stream=width,height".to_string(),
        "-of".to_string(),
        "json".to_string(),
    ];
    if media_path.starts_with('-') {
        args.push(format!("./{media_path}"));
    } else {
        args.push(media_path.to_string());
    }
    args
}

/// Runs ffprobe and summarizes duration + dimensions.
///
/// Returns `Ok(None)` when ffprobe is not available (no explicit path
/// configured and nothing on PATH). `Err` means ffprobe ran but failed or its
/// output could not be parsed. Callers MUST NOT invoke this in dry-run.
pub async fn probe_ffprobe(
    ffprobe_path: Option<&str>,
    media_path: &str,
) -> Result<Option<FFProbeSummary>, String> {
    let media = media_path.trim();
    if media.is_empty() {
        return Err("path_required".to_string());
    }

    let bin = match ffprobe_path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => p.to_string(),
        None => match which_ffprobe().await {
            Some(p) => p,
            None => return Ok(None),
        },
    };

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(&bin)
            .args(probe_args(media))
            .output(),
    )
    .await
    .map_err(|_| "ffprobe_timeout".to_string())?
    .map_err(|e| format!("ffprobe_spawn_failed: {e}"))?;

    if !output.status.success() {
        return Err("ffprobe_failed".to_string());
    }

    parse_ffprobe_output(&String::from_utf8_lossy(&output.stdout))
        .map(Some)
        .ok_or_else(|| "ffprobe_parse_failed".to_string())
}

fn parse_ffprobe_output(stdout: &str) -> Option<FFProbeSummary> {
    #[derive(serde::Deserialize)]
    struct Stream {
        width: Option<i64>,
        height: Option<i64>,
    }
    #[derive(serde::Deserialize)]
    struct Format {
        duration: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Parsed {
        format: Option<Format>,
        streams: Option<Vec<Stream>>,
    }

    let parsed: Parsed = serde_json::from_str(stdout).ok()?;
    let mut summary = FFProbeSummary::default();
    if let Some(dur) = parsed.format.and_then(|f| f.duration) {
        summary.duration_seconds = dur.trim().parse::<f64>().unwrap_or(0.0);
    }
    if let Some(streams) = parsed.streams {
        if let Some(first) = streams.first() {
            summary.width = first.width;
            summary.height = first.height;
        }
    }
    Some(summary)
}

async fn which_ffprobe() -> Option<String> {
    // Minimal PATH lookup without spawning `which`.
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("ffprobe");
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &Path, name: &str, len: usize) -> String {
        let p = dir.join(name);
        std::fs::write(&p, vec![0u8; len]).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn extension_allowlist() {
        assert!(media_extension_allowed(".mkv"));
        assert!(media_extension_allowed(".MKV"));
        assert!(!media_extension_allowed(".exe"));
        assert!(!media_extension_allowed(".txt"));
        assert!(!media_extension_allowed(""));
    }

    #[test]
    fn verify_accepts_large_media_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_file(dir.path(), "movie.mkv", 26 * 1024 * 1024);
        let res = verify_file(&p).unwrap();
        assert!(res.ok, "{}", res.reason);
        assert_eq!(res.reason, "ok");
        assert_eq!(res.file_size, 26 * 1024 * 1024);
    }

    #[test]
    fn verify_rejects_disallowed_extension() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_file(dir.path(), "evil.exe", 30 * 1024 * 1024);
        let res = verify_file(&p).unwrap();
        assert!(!res.ok);
        assert_eq!(res.reason, "extension_not_allowed");
    }

    #[test]
    fn verify_detects_samples_and_trailers() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["sample.mkv", "Movie.Trailer.mkv"] {
            let p = write_file(dir.path(), name, 30 * 1024 * 1024);
            let res = verify_file(&p).unwrap();
            assert!(!res.ok, "{name}");
            assert_eq!(res.reason, "sample_detected", "{name}");
        }
    }

    #[test]
    fn verify_rejects_tiny_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_file(dir.path(), "tiny.mkv", 1024);
        let res = verify_file(&p).unwrap();
        assert!(!res.ok);
        assert_eq!(res.reason, "file_too_small");
    }

    #[test]
    fn verify_errors_on_missing_path_and_dirs() {
        assert!(verify_file("").is_err());
        assert!(verify_file("/nonexistent/nope.mkv").is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(verify_file(dir.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn probe_args_guard_option_like_filenames() {
        let args = probe_args("-oT /etc/passwd");
        assert_eq!(args.last().unwrap(), "./-oT /etc/passwd");
        let args = probe_args("/downloads/movie.mkv");
        assert_eq!(args.last().unwrap(), "/downloads/movie.mkv");
        // Fixed prefix flags are intact.
        assert_eq!(&args[..2], &["-v".to_string(), "error".to_string()]);
    }

    #[test]
    fn parses_ffprobe_json() {
        let out =
            r#"{"format":{"duration":"5400.123000"},"streams":[{"width":3840,"height":2160}]}"#;
        let s = parse_ffprobe_output(out).unwrap();
        assert!((s.duration_seconds - 5400.123).abs() < 1e-6);
        assert_eq!(s.width, Some(3840));
        assert_eq!(s.height, Some(2160));

        let s = parse_ffprobe_output("{}").unwrap();
        assert_eq!(s.duration_seconds, 0.0);
        assert_eq!(s.width, None);

        assert!(parse_ffprobe_output("not json").is_none());
    }
}
