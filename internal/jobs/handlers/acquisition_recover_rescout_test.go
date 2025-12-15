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

func TestRecoverAcquisition_Attempt4EnqueuesRescout(t *testing.T) {
	hash := "cccccccccccccccccccccccccccccccccccccccc"
	// Fake qBittorrent server accepts control calls.
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/api/v2/auth/login":
			w.Header().Set("set-cookie", "SID=abc")
			_, _ = w.Write([]byte("Ok."))
			return
		default:
			// control endpoints return 200
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

	tgt, _ := storage.CreateWantedTarget(ctx, db.SQL, "movie", "tmdb", "1", map[string]any{"query": "x"})
	run, _ := storage.CreateSearchRun(ctx, db.SQL, tgt.ID)
	normS := "{}"
	cand, _ := storage.InsertReleaseCandidate(ctx, db.SQL, run.ID, "ix", `{"title":"x","enclosureUrl":"magnet:?xt=urn:btih:`+hash+`"}`, &normS)
	acq, _ := storage.CreateAcquisition(ctx, db.SQL, cand.ID, tgt.ID, "torrent", "qbittorrent", "default")
	_ = storage.MarkAcquisitionEnqueued(ctx, db.SQL, acq.ID, &hash)

	// Pretend we already attempted recovery 3 times.
	_, _ = db.SQL.ExecContext(ctx, `UPDATE acquisitions SET recovery_attempt = 3 WHERE id = ?`, acq.ID)

	if err := RecoverAcquisition(ctx, RecoverDeps{DB: db.SQL, SecretBox: sb, Policy: security.OutboundPolicy{AllowInternal: true}}, acq.ID); err != nil {
		t.Fatalf("recover: %v", err)
	}

	// Should have enqueued rescout and moved to needs_review.
	acq2, _ := storage.GetAcquisition(ctx, db.SQL, acq.ID)
	if acq2.Status != "needs_review" {
		t.Fatalf("expected needs_review, got %s", acq2.Status)
	}

	var n int
	_ = db.SQL.QueryRowContext(ctx, `SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_rescout'`).Scan(&n)
	if n < 1 {
		t.Fatalf("expected acquisition_rescout job")
	}
}

