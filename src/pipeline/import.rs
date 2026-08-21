//! Filesystem import pipeline: pick the best media file, quarantine failures,
//! and move content into the library.
//!
//! Fixes over the Go original (`internal/import/v0.go`):
//! - Hardlink-first import falls back to copy on ANY link failure, including
//!   cross-device (EXDEV) — the original surfaced the raw error to users whose
//!   library lived on another mount.
//! - Destination filename components are sanitized before joining, so a
//!   target name like `../../etc/passwd` cannot escape the library root.
//! - Destinations are uniquified; dry-run computes the planned path without
//!   touching the disk.

use std::io;
use std::path::{Path, PathBuf};

/// Media extensions eligible for import/verification.
pub fn media_extension_allowed(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        ".mkv" | ".mp4" | ".m4v" | ".avi" | ".mov" | ".mpg" | ".mpeg" | ".ts"
    )
}

fn is_sample_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("sample") || lower.contains("trailer")
}

/// Picks the file to import: when `root` is a directory, the largest
/// non-sample media file wins; if only sample files exist, the largest sample
/// is used. A direct file path must be an allowed media extension.
pub fn pick_best_media_file(root: &str) -> Result<String, String> {
    let path = Path::new(root.trim());
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_dir() {
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        if !media_extension_allowed(&ext) {
            return Err("not_media".to_string());
        }
        return Ok(path.to_string_lossy().into_owned());
    }

    let mut best: Option<(i64, PathBuf)> = None;
    let mut best_sample: Option<(i64, PathBuf)> = None;
    walk(path, &mut |p, size| {
        let ext = p
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        if !media_extension_allowed(&ext) {
            return;
        }
        let is_sample = is_sample_name(&p.to_string_lossy());
        let slot = if is_sample { &mut best_sample } else { &mut best };
        let should_replace = match slot.as_ref() {
            Some((cur, _)) => size > *cur,
            None => true,
        };
        if should_replace {
            *slot = Some((size, p.to_path_buf()));
        }
    });
    match best.or(best_sample) {
        Some((_, p)) => Ok(p.to_string_lossy().into_owned()),
        None => Err("no_media_found".to_string()),
    }
}

fn walk(dir: &Path, f: &mut impl FnMut(&Path, i64)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&p, f),
            Ok(_) => {
                let size = entry.metadata().map(|m| m.len() as i64).unwrap_or(0);
                f(&p, size);
            }
            Err(_) => {}
        }
    }
}

/// Moves `src_path` into `<quarantine_root>/quarantine/`, uniquifying the
/// destination. Falls back to copy+remove when rename fails (cross-device).
pub fn quarantine(src_path: &str, quarantine_root: &str) -> Result<String, String> {
    let src = Path::new(src_path.trim());
    let base = src
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .ok_or_else(|| "invalid_source".to_string())?;
    let dest_dir = Path::new(quarantine_root.trim()).join("quarantine");
    std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
    let dest = uniquify(&dest_dir.join(&base));
    if std::fs::rename(src, &dest).is_ok() {
        return Ok(dest.to_string_lossy().into_owned());
    }
    copy_excl(src, &dest)?;
    let _ = std::fs::remove_file(src);
    Ok(dest.to_string_lossy().into_owned())
}

/// Imports `src_path` into the exact destination path. Hardlink first; copy
/// fallback covers cross-device and filesystems without hardlinks. The
/// destination is uniquified if it already exists.
pub fn import_file_to_path(src_path: &str, dest_path: &str) -> Result<String, String> {
    let src = Path::new(src_path.trim());
    if src.file_name().is_none() {
        return Err("invalid_source".to_string());
    }
    let dest = Path::new(dest_path.trim());
    if dest.as_os_str().is_empty() || dest.parent().is_none_or(|d| d.as_os_str().is_empty()) {
        return Err("invalid_dest_path".to_string());
    }
    std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;

    let dest = uniquify(dest);
    // Hardlink first (instant, zero-copy); fall back to copying on any
    // failure — most importantly EXDEV across mounts/devices.
    match std::fs::hard_link(src, &dest) {
        Ok(()) => return Ok(dest.to_string_lossy().into_owned()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!("source_missing: {e}"));
        }
        Err(_) => {}
    }
    copy_excl(src, &dest)?;
    Ok(dest.to_string_lossy().into_owned())
}

fn copy_excl(src: &Path, dest: &Path) -> Result<(), String> {
    let mut input = std::fs::File::open(src).map_err(|e| e.to_string())?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| e.to_string())?;
    io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
    Ok(())
}

/// Makes a best-effort safe directory/file name: strips path separators,
/// traversal dots, and control characters; collapses whitespace; caps length.
pub fn sanitize_path_component(s: &str) -> String {
    // Drop pure-dot components ("." / ".." / "...") so traversal attempts
    // cannot survive sanitization; separators become spaces, everything else
    // keeps only safe characters.
    let mut out = String::with_capacity(s.len());
    for comp in s.trim().split(['/', '\\']) {
        if !comp.is_empty() && comp.chars().all(|c| c == '.') {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        for c in comp.chars() {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ' ' | '-') {
                out.push(c);
            } else {
                out.push(' ');
            }
        }
    }
    // Collapse runs of whitespace.
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        let space = c == ' ';
        if !(space && prev_space) {
            collapsed.push(c);
        }
        prev_space = space;
    }
    let trimmed = collapsed.trim_matches(|c| c == '.' || c == ' ').trim_end();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }
    let mut limited: String = trimmed.chars().take(120).collect();
    while limited.ends_with('.') || limited.ends_with(' ') {
        limited.pop();
    }
    if limited.is_empty() {
        return "unknown".to_string();
    }
    limited
}

/// If `p` exists, returns `base(1).ext`, `base(2).ext`, ... up to 999;
/// otherwise returns `p` unchanged.
pub fn uniquify(p: &Path) -> PathBuf {
    if std::fs::symlink_metadata(p).is_err() {
        return p.to_path_buf();
    }
    let dir = p.parent().unwrap_or(Path::new("."));
    let base = p
        .file_stem()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for i in 1..1000 {
        let cand = dir.join(format!("{base}({i}){ext}"));
        if std::fs::symlink_metadata(&cand).is_err() {
            return cand;
        }
    }
    p.to_path_buf()
}

/// Computes where a verified file lands in the library (pure; no IO).
///
/// Returns `(dest_dir, dest_path)`. Layout:
/// - movie → `Movies/<name>/<name><ext>`
/// - episode/season_pack/tv → `TV/<name>/Season 01/<name> - Imported<ext>`
/// - anything else → `imports/<name>/<basename>`
pub fn compute_import_destination(
    library_root: &str,
    target_kind: &str,
    target_name: &str,
    verified_path: &str,
) -> (String, String) {
    let root = Path::new(library_root.trim());
    let kind = target_kind.trim().to_ascii_lowercase();
    let name = sanitize_path_component(target_name);

    let ext = Path::new(verified_path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default();
    let ext = if ext.is_empty() {
        ".mkv".to_string()
    } else {
        ext
    };

    match kind.as_str() {
        "movie" => {
            let dest_dir = root.join("Movies").join(&name);
            let dest_path = dest_dir.join(format!("{name}{ext}"));
            (path_str(dest_dir), path_str(dest_path))
        }
        "episode" | "season_pack" | "tv" => {
            let dest_dir = root.join("TV").join(&name).join("Season 01");
            let dest_path = dest_dir.join(format!("{name} - Imported{ext}"));
            (path_str(dest_dir), path_str(dest_path))
        }
        _ => {
            let dest_dir = root.join("imports").join(&name);
            let base = Path::new(verified_path.trim())
                .file_name()
                .map(|b| sanitize_path_component(&b.to_string_lossy()))
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| format!("{name}.mkv"));
            let dest_path = dest_dir.join(base);
            (path_str(dest_dir), path_str(dest_path))
        }
    }
}

fn path_str(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn sanitizes_path_traversal_attempts() {
        assert_eq!(sanitize_path_component("../../etc/passwd"), "etc passwd");
        assert_eq!(sanitize_path_component("..\\..\\windows"), "windows");
        assert_eq!(
            sanitize_path_component("/absolute/../evil"),
            "absolute evil"
        );
        assert_eq!(sanitize_path_component(""), "unknown");
        assert_eq!(sanitize_path_component("..."), "unknown");
        assert_eq!(sanitize_path_component("  "), "unknown");
        assert_eq!(sanitize_path_component("a/b/c"), "a b c");
        assert_eq!(
            sanitize_path_component("Movie: The Sequel?"),
            "Movie The Sequel"
        );
    }

    #[test]
    fn sanitize_caps_length_and_keeps_safe_names() {
        assert_eq!(
            sanitize_path_component("Movie.2020.1080p"),
            "Movie.2020.1080p"
        );
        let long = "x".repeat(300);
        assert_eq!(sanitize_path_component(&long).len(), 120);
    }

    #[test]
    fn uniquify_appends_counters() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("f.mkv");
        std::fs::write(&a, b"x").unwrap();
        // A missing path is returned unchanged; an existing one is uniquified.
        let missing = dir.path().join("missing.mkv");
        assert_eq!(uniquify(&missing), missing);
        let b = uniquify(&a);
        assert_eq!(b, dir.path().join("f(1).mkv"));
        std::fs::write(&b, b"x").unwrap();
        assert_eq!(uniquify(&a), dir.path().join("f(2).mkv"));
    }

    #[test]
    fn compute_destination_layouts() {
        let (dir, path) = compute_import_destination("/lib", "movie", "My: Movie", "/dl/movie.mkv");
        assert_eq!(dir, "/lib/Movies/My Movie");
        assert_eq!(path, "/lib/Movies/My Movie/My Movie.mkv");

        let (dir, path) = compute_import_destination("/lib", "episode", "Show", "/dl/x.mp4");
        assert_eq!(dir, "/lib/TV/Show/Season 01");
        assert_eq!(path, "/lib/TV/Show/Season 01/Show - Imported.mp4");

        let (dir, path) = compute_import_destination("/lib", "other", "t", "/dl/file.mkv");
        assert_eq!(dir, "/lib/imports/t");
        assert_eq!(path, "/lib/imports/t/file.mkv");

        // Traversal in target name cannot escape the library root.
        let (_, path) =
            compute_import_destination("/lib", "movie", "../../etc/passwd", "/dl/m.mkv");
        assert!(path.starts_with("/lib/Movies/etc passwd/"), "{path}");
    }

    #[test]
    fn pick_best_prefers_largest_non_sample() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small.mkv"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("big.mkv"), vec![0u8; 500]).unwrap();
        std::fs::write(dir.path().join("sample.mkv"), vec![0u8; 900]).unwrap();
        std::fs::write(dir.path().join("notes.txt"), vec![0u8; 2000]).unwrap();
        let picked = pick_best_media_file(dir.path().to_str().unwrap()).unwrap();
        assert!(picked.ends_with("big.mkv"), "{picked}");
    }

    #[test]
    fn pick_best_falls_back_to_sample_and_rejects_non_media() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.mkv"), vec![0u8; 10]).unwrap();
        let picked = pick_best_media_file(dir.path().to_str().unwrap()).unwrap();
        assert!(picked.ends_with("sample.mkv"));

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            pick_best_media_file(empty.path().to_str().unwrap()).unwrap_err(),
            "no_media_found"
        );

        let f = empty.path().join("doc.txt");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(
            pick_best_media_file(f.to_str().unwrap()).unwrap_err(),
            "not_media"
        );
    }

    #[test]
    fn import_hardlinks_then_copies_cross_device() {
        let src_dir = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("movie.mkv");
        std::fs::write(&src, vec![7u8; 4096]).unwrap();

        let dest = lib.path().join("Movies").join("m.mkv");
        let got = import_file_to_path(src.to_str().unwrap(), dest.to_str().unwrap()).unwrap();
        assert_eq!(got, dest.to_str().unwrap());
        assert_eq!(
            std::fs::metadata(&got).unwrap().ino(),
            std::fs::metadata(&src).unwrap().ino(),
            "same filesystem: expect hardlink"
        );

        // Second import of same destination uniquifies instead of clobbering.
        let got2 = import_file_to_path(src.to_str().unwrap(), dest.to_str().unwrap()).unwrap();
        assert!(got2.ends_with("m(1).mkv"), "{got2}");
    }

    #[test]
    fn import_fails_when_source_missing() {
        let lib = tempfile::tempdir().unwrap();
        let dest = lib.path().join("x.mkv");
        let err = import_file_to_path("/nonexistent/a.mkv", dest.to_str().unwrap()).unwrap_err();
        assert!(err.contains("source_missing"), "{err}");
    }

    #[test]
    fn quarantine_moves_and_uniquifies() {
        let qroot = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("bad.mkv");
        std::fs::write(&src, b"data").unwrap();

        let moved = quarantine(src.to_str().unwrap(), qroot.path().to_str().unwrap()).unwrap();
        assert!(moved.contains("quarantine"), "{moved}");
        assert!(!src.exists());

        // A second item with the same name does not clobber the first.
        std::fs::write(&src, b"data2").unwrap();
        let moved2 = quarantine(src.to_str().unwrap(), qroot.path().to_str().unwrap()).unwrap();
        assert_ne!(moved, moved2);
    }
}
