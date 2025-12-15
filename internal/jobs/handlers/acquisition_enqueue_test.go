package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/LukeHagar/Rivet/internal/adapters/downloaders/qbittorrent"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

func TestEnqueueAcquisition_QBittorrent_AddURL(t *testing.T) {
	// Fake qBittorrent server
	var sawLogin, sawAdd bool
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/api/v2/auth/login":
			sawLogin = true
			_ = r.ParseForm()
			if r.Form.Get("username") != "u" || r.Form.Get("password") != "p" {
				w.WriteHeader(403)
				return
			}
			w.Header().Set("set-cookie", "SID=abc")
			_, _ = w.Write([]byte("Ok."))
			return
		case r.URL.Path == "/api/v2/torrents/add":
			sawAdd = true
			_ = r.ParseForm()
			if r.Form.Get("urls") != "http://example.test/t.torrent" {
				w.WriteHeader(400)
				return
			}
			_, _ = w.Write([]byte("Ok."))
			return
		default:
			w.WriteHeader(404)
		}
	}))
	defer srv.Close()

	// Temp DB
	dbPath := filepath.Join(t.TempDir(), "app.db")
	db, err := storage.OpenSQLite("file:" + dbPath)
	if err != nil {
		t.Fatalf("db open: %v", err)
	}
	defer func() { _ = db.Close() }()

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if err := storage.ApplyMigrations(ctx, db.SQL, storage.MigrationsFS(), "migrations"); err != nil {
		t.Fatalf("migrate: %v", err)
	}

	// Seed minimal target/run/candidate
	tgt, err := storage.CreateWantedTarget(ctx, db.SQL, "movie", "tmdb", "550", map[string]any{"query": "x"})
	if err != nil {
		t.Fatalf("target: %v", err)
	}
	run, err := storage.CreateSearchRun(ctx, db.SQL, tgt.ID)
	if err != nil {
		t.Fatalf("run: %v", err)
	}
	raw := map[string]any{"title": "Test 1080p", "enclosureUrl": "http://example.test/t.torrent"}
	rawB, _ := json.Marshal(raw)
	normS := "{}"
	cand, err := storage.InsertReleaseCandidate(ctx, db.SQL, run.ID, "ix", string(rawB), &normS)
	if err != nil {
		t.Fatalf("candidate: %v", err)
	}

	// Configure qBittorrent client in settings (encrypted)
	keyB64 := os.Getenv("APP_SECRET_KEY")
	sb, _ := security.NewSecretBox(keyB64)
	pwEnc, _ := sb.EncryptToString([]byte("p"))
	clientsJSON := `{"qbittorrent":{"url":"` + srv.URL + `","username":"u","passwordEnc":"` + pwEnc + `"}}`
	if err := storage.UpsertSetting(ctx, db.SQL, "setup.clients", clientsJSON); err != nil {
		t.Fatalf("settings: %v", err)
	}

	// Create acquisition
	a, err := storage.CreateAcquisition(ctx, db.SQL, cand.ID, tgt.ID, "torrent", "qbittorrent", "default")
	if err != nil {
		t.Fatalf("acq: %v", err)
	}

	// Run handler with real qbittorrent client
	err = EnqueueAcquisition(ctx, AcquisitionDeps{
		DB:        db.SQL,
		SecretBox: sb,
		Policy:    security.OutboundPolicy{AllowInternal: true},
		NewQb: func(cfg qbittorrent.Config, policy security.OutboundPolicy) (qbAdder, error) {
			// Ensure url is exactly our server
			if !strings.Contains(cfg.BaseURL, srv.URL) {
				t.Fatalf("unexpected base url: %s", cfg.BaseURL)
			}
			return qbittorrent.New(cfg, policy)
		},
	}, a.ID)
	if err != nil {
		t.Fatalf("enqueue: %v", err)
	}

	if !sawLogin || !sawAdd {
		t.Fatalf("expected login and add calls, got login=%v add=%v", sawLogin, sawAdd)
	}

	// Acquisition should be enqueued
	a2, err := storage.GetAcquisition(ctx, db.SQL, a.ID)
	if err != nil {
		t.Fatalf("get acq: %v", err)
	}
	if a2.Status != "enqueued" {
		t.Fatalf("expected status enqueued, got %s", a2.Status)
	}
}

func TestEnqueueAcquisition_MissingURL_Fails(t *testing.T) {
	dbPath := filepath.Join(t.TempDir(), "app.db")
	db, err := storage.OpenSQLite("file:" + dbPath)
	if err != nil {
		t.Fatalf("db open: %v", err)
	}
	defer func() { _ = db.Close() }()

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if err := storage.ApplyMigrations(ctx, db.SQL, storage.MigrationsFS(), "migrations"); err != nil {
		t.Fatalf("migrate: %v", err)
	}

	tgt, _ := storage.CreateWantedTarget(ctx, db.SQL, "movie", "tmdb", "1", map[string]any{"query": "x"})
	run, _ := storage.CreateSearchRun(ctx, db.SQL, tgt.ID)
	rawB := []byte(`{"title":"NoURL"}`)
	normS := "{}"
	cand, _ := storage.InsertReleaseCandidate(ctx, db.SQL, run.ID, "ix", string(rawB), &normS)

	keyB64 := os.Getenv("APP_SECRET_KEY")
	sb, _ := security.NewSecretBox(keyB64)
	pwEnc, _ := sb.EncryptToString([]byte("p"))
	clientsJSON := `{"qbittorrent":{"url":"http://localhost:1234","username":"u","passwordEnc":"` + pwEnc + `"}}`
	_ = storage.UpsertSetting(ctx, db.SQL, "setup.clients", clientsJSON)

	a, _ := storage.CreateAcquisition(ctx, db.SQL, cand.ID, tgt.ID, "torrent", "qbittorrent", "default")
	err = EnqueueAcquisition(ctx, AcquisitionDeps{DB: db.SQL, SecretBox: sb, Policy: security.OutboundPolicy{AllowInternal: true}, NewQb: func(cfg qbittorrent.Config, policy security.OutboundPolicy) (qbAdder, error) {
		return &fakeQb{}, nil
	}}, a.ID)
	if err == nil {
		t.Fatalf("expected error")
	}
}

func TestEnqueueAcquisition_AddURLFails_EnqueuesRescout(t *testing.T) {
	dbPath := filepath.Join(t.TempDir(), "app.db")
	db, err := storage.OpenSQLite("file:" + dbPath)
	if err != nil {
		t.Fatalf("db open: %v", err)
	}
	defer func() { _ = db.Close() }()

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if err := storage.ApplyMigrations(ctx, db.SQL, storage.MigrationsFS(), "migrations"); err != nil {
		t.Fatalf("migrate: %v", err)
	}

	tgt, _ := storage.CreateWantedTarget(ctx, db.SQL, "movie", "tmdb", "1", map[string]any{"query": "x"})
	run, _ := storage.CreateSearchRun(ctx, db.SQL, tgt.ID)
	rawB := []byte(`{"title":"HasURL","enclosureUrl":"magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}`)
	normS := "{}"
	cand, _ := storage.InsertReleaseCandidate(ctx, db.SQL, run.ID, "ix", string(rawB), &normS)

	keyB64 := os.Getenv("APP_SECRET_KEY")
	sb, _ := security.NewSecretBox(keyB64)
	pwEnc, _ := sb.EncryptToString([]byte("p"))
	clientsJSON := `{"qbittorrent":{"url":"http://localhost:1234","username":"u","passwordEnc":"` + pwEnc + `"}}`
	_ = storage.UpsertSetting(ctx, db.SQL, "setup.clients", clientsJSON)

	a, _ := storage.CreateAcquisition(ctx, db.SQL, cand.ID, tgt.ID, "torrent", "qbittorrent", "default")
	err = EnqueueAcquisition(ctx, AcquisitionDeps{
		DB:        db.SQL,
		SecretBox: sb,
		Policy:    security.OutboundPolicy{AllowInternal: true},
		NewQb: func(cfg qbittorrent.Config, policy security.OutboundPolicy) (qbAdder, error) {
			return &fakeQbFail{err: errors.New("add_failed")}, nil
		},
	}, a.ID)
	if err == nil {
		t.Fatalf("expected error")
	}

	var n int
	_ = db.SQL.QueryRowContext(ctx, `SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_rescout'`).Scan(&n)
	if n < 1 {
		t.Fatalf("expected acquisition_rescout job")
	}
}

type fakeQb struct{}

func (f *fakeQb) AddURL(ctx context.Context, torrentURL string) error { return nil }

type fakeQbFail struct{ err error }

func (f *fakeQbFail) AddURL(ctx context.Context, torrentURL string) error { return f.err }

var _ = sql.ErrNoRows
