package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

func TestAcquisitionFlow_EnqueueThenMonitorImports(t *testing.T) {
	// Filesystem layout
	downloadsDir := filepath.Join(t.TempDir(), "downloads")
	libraryDir := filepath.Join(t.TempDir(), "library")
	quarantineDir := filepath.Join(t.TempDir(), "quarantine")
	if err := os.MkdirAll(downloadsDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(libraryDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(quarantineDir, 0o755); err != nil {
		t.Fatal(err)
	}

	contentPath := filepath.Join(downloadsDir, "movie.mkv")
	if err := os.WriteFile(contentPath, []byte("data"), 0o644); err != nil {
		t.Fatal(err)
	}

	// Fake qBittorrent
	hash := "0123456789abcdef0123456789abcdef01234567"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/api/v2/auth/login":
			_ = r.ParseForm()
			w.Header().Set("set-cookie", "SID=abc")
			_, _ = w.Write([]byte("Ok."))
			return
		case "/api/v2/torrents/add":
			_ = r.ParseForm()
			_, _ = w.Write([]byte("Ok."))
			return
		case "/api/v2/torrents/info":
			w.Header().Set("content-type", "application/json")
			_, _ = w.Write([]byte(`[{"hash":"` + hash + `","name":"movie","progress":1.0,"state":"uploading","content_path":"` + contentPath + `","save_path":"` + downloadsDir + `"}]`))
			return
		default:
			w.WriteHeader(404)
			return
		}
	}))
	defer srv.Close()

	// Temp DB
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

	// Setup settings
	sb, _ := security.NewSecretBox(os.Getenv("APP_SECRET_KEY"))
	pwEnc, _ := sb.EncryptToString([]byte("p"))
	clientsJSON := `{"qbittorrent":{"url":"` + srv.URL + `","username":"u","passwordEnc":"` + pwEnc + `"}}`
	if err := storage.UpsertSetting(ctx, db.SQL, "setup.clients", clientsJSON); err != nil {
		t.Fatalf("clients: %v", err)
	}
	pathsJSON := `{"downloadsCompletedPath":"` + downloadsDir + `","libraryRoots":["` + libraryDir + `"],"quarantinePath":"` + quarantineDir + `"}`
	if err := storage.UpsertSetting(ctx, db.SQL, "setup.paths", pathsJSON); err != nil {
		t.Fatalf("paths: %v", err)
	}

	// Target/run/candidate
	tgt, _ := storage.CreateWantedTarget(ctx, db.SQL, "movie", "tmdb", "550", map[string]any{"query": "movie"})
	run, _ := storage.CreateSearchRun(ctx, db.SQL, tgt.ID)
	raw := map[string]any{"title": "movie", "enclosureUrl": "magnet:?xt=urn:btih:" + hash}
	rawB, _ := json.Marshal(raw)
	normS := "{}"
	cand, _ := storage.InsertReleaseCandidate(ctx, db.SQL, run.ID, "ix", string(rawB), &normS)

	acq, err := storage.CreateAcquisition(ctx, db.SQL, cand.ID, tgt.ID, "torrent", "qbittorrent", "default")
	if err != nil {
		t.Fatalf("create acq: %v", err)
	}

	pub := &capturePub{}
	// Enqueue acquisition: should store hash and schedule monitor job.
	if err := EnqueueAcquisition(ctx, AcquisitionDeps{DB: db.SQL, SecretBox: sb, Policy: security.OutboundPolicy{AllowInternal: true}, Hub: pub}, acq.ID); err != nil {
		t.Fatalf("enqueue: %v", err)
	}

	acq2, _ := storage.GetAcquisition(ctx, db.SQL, acq.ID)
	if acq2.Status != "enqueued" {
		t.Fatalf("expected enqueued, got %s", acq2.Status)
	}
	if acq2.ExternalID == nil || *acq2.ExternalID != hash {
		t.Fatalf("expected external id hash")
	}

	// Ensure monitor job exists.
	var n int
	if err := db.SQL.QueryRowContext(ctx, `SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_monitor'`).Scan(&n); err != nil {
		t.Fatalf("job count: %v", err)
	}
	if n < 1 {
		t.Fatalf("expected monitor job")
	}

	// Run monitor once: should import.
	if err := MonitorAcquisition(ctx, MonitorDeps{DB: db.SQL, SecretBox: sb, Policy: security.OutboundPolicy{AllowInternal: true}, Hub: pub}, acq.ID); err != nil {
		t.Fatalf("monitor: %v", err)
	}

	acq3, _ := storage.GetAcquisition(ctx, db.SQL, acq.ID)
	if acq3.Status != "imported" {
		t.Fatalf("expected imported, got %s", acq3.Status)
	}
	if acq3.ImportedPath == nil {
		t.Fatalf("expected imported path")
	}
	if _, err := os.Stat(*acq3.ImportedPath); err != nil {
		t.Fatalf("imported file missing: %v", err)
	}
	// Sanity: we emitted some lifecycle events.
	if pub.countPrefix("download.") < 2 {
		t.Fatalf("expected download events")
	}
	if pub.countPrefix("import.") < 2 {
		t.Fatalf("expected import events")
	}
}

func TestMonitorAcquisition_ReschedulesWhenNotComplete(t *testing.T) {
	downloadsDir := filepath.Join(t.TempDir(), "downloads")
	libraryDir := filepath.Join(t.TempDir(), "library")
	quarantineDir := filepath.Join(t.TempDir(), "quarantine")
	_ = os.MkdirAll(downloadsDir, 0o755)
	_ = os.MkdirAll(libraryDir, 0o755)
	_ = os.MkdirAll(quarantineDir, 0o755)

	hash := "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	// Fake qBittorrent returns downloading state.
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
	raw := map[string]any{"title": "x", "enclosureUrl": "magnet:?xt=urn:btih:" + hash}
	rawB, _ := json.Marshal(raw)
	normS := "{}"
	cand, _ := storage.InsertReleaseCandidate(ctx, db.SQL, run.ID, "ix", string(rawB), &normS)
	acq, _ := storage.CreateAcquisition(ctx, db.SQL, cand.ID, tgt.ID, "torrent", "qbittorrent", "default")
	_ = storage.MarkAcquisitionEnqueued(ctx, db.SQL, acq.ID, &hash)

	pub := &capturePub{}
	if err := MonitorAcquisition(ctx, MonitorDeps{DB: db.SQL, SecretBox: sb, Policy: security.OutboundPolicy{AllowInternal: true}, PollEvery: 1 * time.Second, Hub: pub}, acq.ID); err != nil {
		t.Fatalf("monitor: %v", err)
	}

	var n int
	if err := db.SQL.QueryRowContext(ctx, `SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_monitor'`).Scan(&n); err != nil {
		t.Fatalf("job count: %v", err)
	}
	if n < 1 {
		t.Fatalf("expected rescheduled monitor job")
	}
}

var _ = sql.ErrNoRows

type capturePub struct {
	events []string
}

func (c *capturePub) Publish(eventType string, payload any) {
	c.events = append(c.events, eventType)
}

func (c *capturePub) countPrefix(p string) int {
	n := 0
	for _, e := range c.events {
		if len(e) >= len(p) && e[:len(p)] == p {
			n++
		}
	}
	return n
}
