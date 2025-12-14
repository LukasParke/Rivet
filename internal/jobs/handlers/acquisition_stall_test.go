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

func TestMonitorAcquisition_StallEnqueuesRecover(t *testing.T) {
	downloadsDir := filepath.Join(t.TempDir(), "downloads")
	libraryDir := filepath.Join(t.TempDir(), "library")
	quarantineDir := filepath.Join(t.TempDir(), "quarantine")
	_ = os.MkdirAll(downloadsDir, 0o755)
	_ = os.MkdirAll(libraryDir, 0o755)
	_ = os.MkdirAll(quarantineDir, 0o755)

	hash := "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
	// Fake qBittorrent returns downloading, constant progress.
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/api/v2/auth/login":
			w.Header().Set("set-cookie", "SID=abc")
			_, _ = w.Write([]byte("Ok."))
			return
		case "/api/v2/torrents/info":
			w.Header().Set("content-type", "application/json")
			_, _ = w.Write([]byte(`[{"hash":"` + hash + `","name":"x","progress":0.4,"state":"downloading","content_path":"","save_path":"` + downloadsDir + `"}]`))
			return
		default:
			w.WriteHeader(200)
			_, _ = w.Write([]byte("Ok."))
		}
	}))
	defer srv.Close()

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

	sb, _ := security.NewSecretBox(os.Getenv("APP_SECRET_KEY"))
	pwEnc, _ := sb.EncryptToString([]byte("p"))
	clientsJSON := `{"qbittorrent":{"url":"` + srv.URL + `","username":"u","passwordEnc":"` + pwEnc + `"}}`
	_ = storage.UpsertSetting(ctx, db.SQL, "setup.clients", clientsJSON)
	pathsJSON := `{"downloadsCompletedPath":"` + downloadsDir + `","libraryRoots":["` + libraryDir + `"],"quarantinePath":"` + quarantineDir + `"}`
	_ = storage.UpsertSetting(ctx, db.SQL, "setup.paths", pathsJSON)

	tgt, _ := storage.CreateWantedTarget(ctx, db.SQL, "movie", "tmdb", "1", map[string]any{"query": "x"})
	run, _ := storage.CreateSearchRun(ctx, db.SQL, tgt.ID)
	normS := "{}"
	cand, _ := storage.InsertReleaseCandidate(ctx, db.SQL, run.ID, "ix", `{"title":"x","enclosureUrl":"magnet:?xt=urn:btih:`+hash+`"}`, &normS)
	acq, _ := storage.CreateAcquisition(ctx, db.SQL, cand.ID, tgt.ID, "torrent", "qbittorrent", "default")
	_ = storage.MarkAcquisitionEnqueued(ctx, db.SQL, acq.ID, &hash)

	// Force progress_changed_at to be old.
	old := time.Now().UTC().Add(-2 * time.Hour).Format(time.RFC3339Nano)
	_, _ = db.SQL.ExecContext(ctx, `UPDATE acquisitions SET status='downloading', progress=0.4, progress_changed_at=? WHERE id=?`, old, acq.ID)

	pub := &capturePub{}
	if err := MonitorAcquisition(ctx, MonitorDeps{DB: db.SQL, SecretBox: sb, Policy: security.OutboundPolicy{AllowInternal: true}, Hub: pub, StallAfter: 30 * time.Minute, PollEvery: 1 * time.Second}, acq.ID); err != nil {
		t.Fatalf("monitor: %v", err)
	}

	acq2, _ := storage.GetAcquisition(ctx, db.SQL, acq.ID)
	if acq2.Status != "stalled" {
		t.Fatalf("expected stalled, got %s", acq2.Status)
	}

	var n int
	_ = db.SQL.QueryRowContext(ctx, `SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_recover'`).Scan(&n)
	if n < 1 {
		t.Fatalf("expected recover job")
	}
	if pub.countPrefix("download.stalled") < 1 {
		t.Fatalf("expected download.stalled event")
	}
}
