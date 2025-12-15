package importer

import (
	"os"
	"path/filepath"
	"testing"
)

func TestPickBestMediaFile_PrefersNonSample(t *testing.T) {
	dir := t.TempDir()
	// Bigger but sample-like name should be avoided if a non-sample exists.
	sample := filepath.Join(dir, "movie.sample.mkv")
	main := filepath.Join(dir, "movie.mkv")
	if err := os.WriteFile(sample, []byte("x"), 0o644); err != nil {
		t.Fatalf("write sample: %v", err)
	}
	if err := os.WriteFile(main, []byte("y"), 0o644); err != nil {
		t.Fatalf("write main: %v", err)
	}
	// Make sample larger.
	if err := os.Truncate(sample, 10_000); err != nil {
		t.Fatalf("truncate sample: %v", err)
	}
	if err := os.Truncate(main, 100); err != nil {
		t.Fatalf("truncate main: %v", err)
	}

	got, err := PickBestMediaFile(dir)
	if err != nil {
		t.Fatalf("PickBestMediaFile: %v", err)
	}
	if filepath.Clean(got) != filepath.Clean(main) {
		t.Fatalf("expected %q, got %q", main, got)
	}
}

func TestPickBestMediaFile_FallsBackToSampleIfOnlyOption(t *testing.T) {
	dir := t.TempDir()
	sample := filepath.Join(dir, "trailer.mp4")
	if err := os.WriteFile(sample, []byte("x"), 0o644); err != nil {
		t.Fatalf("write sample: %v", err)
	}

	got, err := PickBestMediaFile(dir)
	if err != nil {
		t.Fatalf("PickBestMediaFile: %v", err)
	}
	if filepath.Clean(got) != filepath.Clean(sample) {
		t.Fatalf("expected %q, got %q", sample, got)
	}
}

