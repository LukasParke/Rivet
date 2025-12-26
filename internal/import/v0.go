package importer

import (
	"errors"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

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

// PickBestMediaFile returns a file path to import.
func PickBestMediaFile(root string) (string, error) {
	st, err := os.Stat(root)
	if err != nil {
		return "", err
	}
	if !st.IsDir() {
		if !mediaExtAllow[strings.ToLower(filepath.Ext(root))] {
			return "", errors.New("not_media")
		}
		return root, nil
	}

	// Prefer the largest non-sample media file; fall back to the largest media file.
	var bestPath string
	var bestSize int64 = -1
	var bestSamplePath string
	var bestSampleSize int64 = -1
	_ = filepath.WalkDir(root, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if d.IsDir() {
			return nil
		}
		ext := strings.ToLower(filepath.Ext(p))
		if !mediaExtAllow[ext] {
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return nil
		}
		name := strings.ToLower(filepath.Base(p))
		isSample := strings.Contains(name, "sample") || strings.Contains(name, "trailer")
		if isSample {
			if info.Size() > bestSampleSize {
				bestSampleSize = info.Size()
				bestSamplePath = p
			}
			return nil
		}
		if info.Size() > bestSize {
			bestSize = info.Size()
			bestPath = p
		}
		return nil
	})
	if bestPath == "" {
		if bestSamplePath != "" {
			return bestSamplePath, nil
		}
		return "", errors.New("no_media_found")
	}
	return bestPath, nil
}

// Quarantine moves srcPath into quarantineRoot/quarantine/ and returns new path.
func Quarantine(srcPath, quarantineRoot string) (string, error) {
	srcPath = filepath.Clean(srcPath)
	quarantineRoot = filepath.Clean(quarantineRoot)
	base := filepath.Base(srcPath)
	if base == "." || base == string(filepath.Separator) {
		return "", errors.New("invalid_source")
	}
	destDir := filepath.Join(quarantineRoot, "quarantine")
	if err := os.MkdirAll(destDir, 0o755); err != nil {
		return "", err
	}
	dest := uniquify(filepath.Join(destDir, base))
	if err := os.Rename(srcPath, dest); err == nil {
		return dest, nil
	}
	// Fallback: copy + remove.
	in, err := os.Open(srcPath)
	if err != nil {
		return "", err
	}
	defer in.Close()
	out, err := os.OpenFile(dest, os.O_CREATE|os.O_WRONLY|os.O_EXCL, 0o644)
	if err != nil {
		return "", err
	}
	defer func() { _ = out.Close() }()
	if _, err := io.Copy(out, in); err != nil {
		return "", err
	}
	_ = os.Remove(srcPath)
	return dest, nil
}

// ImportFile attempts hardlink then copy.
func ImportFile(srcPath, libraryRoot string) (string, error) {
	srcPath = filepath.Clean(srcPath)
	libraryRoot = filepath.Clean(libraryRoot)

	destDir := filepath.Join(libraryRoot, "imports")
	return ImportFileToDir(srcPath, destDir)
}

// ImportFileToDir attempts hardlink then copy into destDir.
func ImportFileToDir(srcPath, destDir string) (string, error) {
	base := filepath.Base(srcPath)
	if base == "." || base == string(filepath.Separator) {
		return "", errors.New("invalid_source")
	}

	destDir = filepath.Clean(destDir)
	if destDir == "" || destDir == "." || destDir == string(filepath.Separator) {
		return "", errors.New("invalid_dest_dir")
	}
	if err := os.MkdirAll(destDir, 0o755); err != nil {
		return "", err
	}

	dest := filepath.Join(destDir, base)
	dest = uniquify(dest)

	if err := os.Link(srcPath, dest); err == nil {
		return dest, nil
	}

	// Copy fallback
	in, err := os.Open(srcPath)
	if err != nil {
		return "", err
	}
	defer in.Close()

	out, err := os.OpenFile(dest, os.O_CREATE|os.O_WRONLY|os.O_EXCL, 0o644)
	if err != nil {
		return "", err
	}
	defer func() { _ = out.Close() }()

	if _, err := io.Copy(out, in); err != nil {
		return "", err
	}
	return dest, nil
}

// ImportFileToPath attempts hardlink then copy into the exact destination path.
// If the destination already exists, it will be uniquified.
func ImportFileToPath(srcPath, destPath string) (string, error) {
	srcPath = filepath.Clean(srcPath)
	destPath = filepath.Clean(destPath)

	base := filepath.Base(srcPath)
	if base == "." || base == string(filepath.Separator) {
		return "", errors.New("invalid_source")
	}

	if destPath == "" || destPath == "." || destPath == string(filepath.Separator) {
		return "", errors.New("invalid_dest_path")
	}
	destDir := filepath.Dir(destPath)
	if destDir == "" || destDir == "." || destDir == string(filepath.Separator) {
		return "", errors.New("invalid_dest_dir")
	}
	if err := os.MkdirAll(destDir, 0o755); err != nil {
		return "", err
	}

	destPath = uniquify(destPath)

	if err := os.Link(srcPath, destPath); err == nil {
		return destPath, nil
	}

	// Copy fallback
	in, err := os.Open(srcPath)
	if err != nil {
		return "", err
	}
	defer in.Close()

	out, err := os.OpenFile(destPath, os.O_CREATE|os.O_WRONLY|os.O_EXCL, 0o644)
	if err != nil {
		return "", err
	}
	defer func() { _ = out.Close() }()

	if _, err := io.Copy(out, in); err != nil {
		return "", err
	}
	return destPath, nil
}

var sanitizeRe = regexp.MustCompile(`[^a-zA-Z0-9._ -]+`)

// SanitizePathComponent makes a best-effort safe directory/file name.
func SanitizePathComponent(s string) string {
	s = strings.TrimSpace(s)
	if s == "" {
		return "unknown"
	}
	s = sanitizeRe.ReplaceAllString(s, " ")
	s = strings.TrimSpace(s)
	s = strings.Join(strings.Fields(s), " ")
	s = strings.Trim(s, ". ")
	if s == "" {
		return "unknown"
	}
	if len(s) > 120 {
		s = s[:120]
		s = strings.TrimSpace(s)
	}
	return s
}

func uniquify(p string) string {
	// If it already exists, add (1), (2), ...
	if _, err := os.Stat(p); err != nil {
		return p
	}
	ext := filepath.Ext(p)
	base := strings.TrimSuffix(filepath.Base(p), ext)
	dir := filepath.Dir(p)
	for i := 1; i < 1000; i++ {
		cand := filepath.Join(dir, base+"("+itoa(i)+")"+ext)
		if _, err := os.Stat(cand); err != nil {
			return cand
		}
	}
	return p
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	buf := make([]byte, 0, 16)
	for n > 0 {
		d := n % 10
		buf = append(buf, byte('0'+d))
		n /= 10
	}
	for i, j := 0, len(buf)-1; i < j; i, j = i+1, j-1 {
		buf[i], buf[j] = buf[j], buf[i]
	}
	return string(buf)
}
