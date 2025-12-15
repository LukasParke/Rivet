package verify

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
)

// Very small MVP verifier.
// Later: ffprobe/mediainfo, hashes, season pack completeness, malware scan.

var mediaExtAllow = map[string]bool{
	".mkv":  true,
	".mp4":  true,
	".m4v":  true,
	".avi":  true,
	".mov":  true,
	".mpg":  true,
	".mpeg": true,
	".ts":   true,
}

type Result struct {
	OK        bool
	Reason    string
	FilePath  string
	FileSize  int64
}

func VerifyFile(path string) (Result, error) {
	path = filepath.Clean(strings.TrimSpace(path))
	if path == "" {
		return Result{}, errors.New("path_required")
	}
	st, err := os.Stat(path)
	if err != nil {
		return Result{}, err
	}
	if st.IsDir() {
		return Result{}, errors.New("not_a_file")
	}

	ext := strings.ToLower(filepath.Ext(path))
	if !mediaExtAllow[ext] {
		return Result{OK: false, Reason: "extension_not_allowed", FilePath: path, FileSize: st.Size()}, nil
	}
	name := strings.ToLower(filepath.Base(path))
	if strings.Contains(name, "sample") || strings.Contains(name, "trailer") {
		return Result{OK: false, Reason: "sample_detected", FilePath: path, FileSize: st.Size()}, nil
	}
	// Avoid super tiny files (common for samples, broken downloads).
	const minSize = 25 * 1024 * 1024
	if st.Size() < minSize {
		return Result{OK: false, Reason: "file_too_small", FilePath: path, FileSize: st.Size()}, nil
	}

	return Result{OK: true, Reason: "ok", FilePath: path, FileSize: st.Size()}, nil
}
