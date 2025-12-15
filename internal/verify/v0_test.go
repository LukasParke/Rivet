package verify

import (
	"os"
	"path/filepath"
	"testing"
)

func TestVerifyFile_DetectsSampleByName(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "Movie.Sample.mkv")
	if err := os.WriteFile(p, []byte("x"), 0o644); err != nil {
		t.Fatalf("write: %v", err)
	}
	// Large enough to not trip size gate.
	if err := os.Truncate(p, 30*1024*1024); err != nil {
		t.Fatalf("truncate: %v", err)
	}
	res, err := VerifyFile(p)
	if err != nil {
		t.Fatalf("VerifyFile: %v", err)
	}
	if res.OK || res.Reason != "sample_detected" {
		t.Fatalf("expected sample_detected, got ok=%v reason=%q", res.OK, res.Reason)
	}
}

func TestVerifyFile_TooSmall(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "Movie.mkv")
	if err := os.WriteFile(p, []byte("x"), 0o644); err != nil {
		t.Fatalf("write: %v", err)
	}
	if err := os.Truncate(p, 1*1024*1024); err != nil {
		t.Fatalf("truncate: %v", err)
	}
	res, err := VerifyFile(p)
	if err != nil {
		t.Fatalf("VerifyFile: %v", err)
	}
	if res.OK || res.Reason != "file_too_small" {
		t.Fatalf("expected file_too_small, got ok=%v reason=%q", res.OK, res.Reason)
	}
}

