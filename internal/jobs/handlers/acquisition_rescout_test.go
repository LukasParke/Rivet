package handlers

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

func TestRescoutAcquisition_EnqueuesReplacementExcludingCurrentHash(t *testing.T) {
	hash1 := "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	hash2 := "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
	hash3 := "cccccccccccccccccccccccccccccccccccccccc"

	// Fake indexer server (newznab/torznab compatible enough for our parser).
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/api" {
			w.WriteHeader(404)
			return
		}
		w.Header().Set("content-type", "application/xml")
		body := `<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <item>
      <title>Test 1080p</title>
      <guid>g1</guid>
      <link>magnet:?xt=urn:btih:` + hash1 + `</link>
      <enclosure url="magnet:?xt=urn:btih:` + hash1 + `" />
    </item>
    <item>
      <title>Test 2160p</title>
      <guid>g2</guid>
      <link>magnet:?xt=urn:btih:` + hash2 + `</link>
      <enclosure url="magnet:?xt=urn:btih:` + hash2 + `" />
    </item>
    <item>
      <title>Test 720p</title>
      <guid>g3</guid>
      <link>magnet:?xt=urn:btih:` + hash3 + `</link>
      <enclosure url="magnet:?xt=urn:btih:` + hash3 + `" />
    </item>
  </channel>
</rss>`
		_, _ = w.Write([]byte(body))
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
	apiKeyEnc, _ := sb.EncryptToString([]byte("k"))
	indexersJSON := `{"torznab":[{"name":"ix","url":"` + srv.URL + `","apiKeyEnc":"` + apiKeyEnc + `","username":"","passwordEnc":""}],"newznab":[]}`
	_ = storage.UpsertSetting(ctx, db.SQL, "setup.indexers", indexersJSON)

	// Seed target and existing acquisition using hash1.
	tgt, _ := storage.CreateWantedTarget(ctx, db.SQL, "movie", "tmdb", "1", map[string]any{"query": "x"})
	run, _ := storage.CreateSearchRun(ctx, db.SQL, tgt.ID)
	normS := "{}"
	rawOld := map[string]any{"title": "Old", "enclosureUrl": "magnet:?xt=urn:btih:" + hash1}
	rawOldB, _ := json.Marshal(rawOld)
	candOld, _ := storage.InsertReleaseCandidate(ctx, db.SQL, run.ID, "ix", string(rawOldB), &normS)
	acqOld, _ := storage.CreateAcquisition(ctx, db.SQL, candOld.ID, tgt.ID, "torrent", "qbittorrent", "default")
	_ = storage.MarkAcquisitionEnqueued(ctx, db.SQL, acqOld.ID, &hash1)
	_ = storage.MarkAcquisitionNeedsReview(ctx, db.SQL, acqOld.ID, "stalled", nil, "stalled")

	// Blacklist hash2 so rescout must pick hash3.
	_, _ = storage.InsertBlacklistEntry(ctx, db.SQL, storage.BlacklistEntry{
		Scope:      "target",
		TargetID:   &tgt.ID,
		MatchType:  "hash",
		MatchValue: hash2,
	})

	// Run rescout.
	if err := RescoutAcquisition(ctx, RescoutDeps{DB: db.SQL, SecretBox: sb, Policy: security.OutboundPolicy{AllowInternal: true}}, acqOld.ID); err != nil {
		t.Fatalf("rescout: %v", err)
	}

	// Should enqueue a replacement acquisition job.
	var enqueueJobs int
	_ = db.SQL.QueryRowContext(ctx, `SELECT COUNT(1) FROM jobs WHERE kind = 'acquisition_enqueue'`).Scan(&enqueueJobs)
	if enqueueJobs < 1 {
		t.Fatalf("expected acquisition_enqueue job")
	}

	// Should create a second acquisition for a different candidate (hash2).
	var newID string
	_ = db.SQL.QueryRowContext(ctx, `SELECT id FROM acquisitions WHERE id != ? ORDER BY created_at DESC LIMIT 1`, acqOld.ID).Scan(&newID)
	if newID == "" {
		t.Fatalf("expected replacement acquisition")
	}

	na, err := storage.GetAcquisition(ctx, db.SQL, newID)
	if err != nil {
		t.Fatalf("get new acq: %v", err)
	}
	if na.CandidateID == candOld.ID {
		t.Fatalf("expected different candidate")
	}
	// Ensure replacement candidate magnet hash is not the excluded one.
	row := db.SQL.QueryRowContext(ctx, `SELECT raw_json FROM release_candidates WHERE id = ?`, na.CandidateID)
	var rawNew string
	_ = row.Scan(&rawNew)
	if strings.Contains(rawNew, hash1) {
		t.Fatalf("expected not to pick excluded hash")
	}
	if strings.Contains(rawNew, hash2) {
		t.Fatalf("expected not to pick blacklisted hash")
	}
}

