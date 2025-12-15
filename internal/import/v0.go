package importer

import (
	"errors"
	"io"
	"os"
	"path/filepath"
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

	var bestPath string
	var bestSize int64 = -1
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
		if info.Size() > bestSize {
			bestSize = info.Size()
			bestPath = p
		}
		return nil
	})
	if bestPath == "" {
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

	base := filepath.Base(srcPath)
	if base == "." || base == string(filepath.Separator) {
		return "", errors.New("invalid_source")
	}

	destDir := filepath.Join(libraryRoot, "imports")
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
