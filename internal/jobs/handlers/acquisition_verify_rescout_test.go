package handlers

import (
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

func TestVerifyAcquisition_FailureEnqueuesRescout(t *testing.T) {
	downloadsDir := filepath.Join(t.TempDir(), "downloads")
	libraryDir := filepath.Join(t.TempDir(), "library")
	quarantineDir := filepath.Join(t.TempDir(), "quarantine")
	_ = os.MkdirAll(downloadsDir, 0o755)
	_ = os.MkdirAll(libraryDir, 0o755)
	_ = os.MkdirAll(quarantineDir, 0o755)

	// Create a "sample" media file that will fail verification by name.
	contentDir := filepath.Join(downloadsDir, "job1")
	_ = os.MkdirAll(contentDir, 0o755)
	p := filepath.Join(contentDir, "movie.sample.mkv")
	_ = os.WriteFile(p, []byte("x"), 0o644)
	_ = os.Truncate(p, 50*1024*1024) // large enough; still fails by sample name

	db, err := storage.OpenSQLite("file:" + filepath.Join(t.TempDir(), "app.db"))
	if err != nil {
		t.Fatalf("db open: %v", err)
	}
	defer func() { _ = db.Close() }()

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if err := storage.ApplyMigrations(ctx, db.SQL, storage.MigrationsFS(), "migrations"); err != nil {
		t.Fatalf("migrate: %v", err)
	}

	pathsJSON := `{"downloadsCompletedPath":"` + downloadsDir + `","libraryRoots":["` + libraryDir + `"],"quarantinePath":"` + quarantineDir + `"}`
	_ = storage.UpsertSetting(ctx, db.SQL, "setup.paths", pathsJSON)

	// We don't need real indexers here; we only assert the rescout job is enqueued.
	sb, _ := security.NewSecretBox(os.Getenv("APP_SECRET_KEY"))
	apiKeyEnc, _ := sb.EncryptToString([]byte("k"))
	indexerSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("content-type", "application/xml")
		_, _ = w.Write([]byte(`<rss version="2.0"><channel></channel></rss>`))
	}))
	defer indexerSrv.Close()
	indexersJSON := `{"torznab":[{"name":"ix","url":"` + indexerSrv.URL + `","apiKeyEnc":"` + apiKeyEnc + `","username":"","passwordEnc":""}],"newznab":[]}`
	_ = storage.UpsertSetting(ctx, db.SQL, "setup.indexers", indexersJSON)

	tgt, _ := storage.CreateWantedTarget(ctx, db.SQL, "movie", "tmdb", "1", map[string]any{"query": "x"})
	run, _ := storage.CreateSearchRun(ctx, db.SQL, tgt.ID)
	normS := "{}"
	cand, _ := storage.InsertReleaseCandidate(ctx, db.SQL, run.ID, "ix", `{"title":"x","enclosureUrl":"magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}`, &normS)
	acq, _ := storage.CreateAcquisition(ctx, db.SQL, cand.ID, tgt.ID, "torrent", "qbittorrent", "default")
	_ = storage.MarkAcquisitionCompleted(ctx, db.SQL, acq.ID, strPtr(contentDir))

	_ = VerifyAcquisition(ctx, VerifyDeps{DB: db.SQL}, acq.ID)

	var n int
	_ = db.SQL.QueryRowContext(ctx, `SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_rescout'`).Scan(&n)
	if n < 1 {
		t.Fatalf("expected acquisition_rescout job")
	}
}

