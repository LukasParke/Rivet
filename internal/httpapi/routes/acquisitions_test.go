package routes

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/storage"
)

func TestGetAcquisitionByID(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	db, err := storage.OpenSQLite("file::memory:")
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = db.Close() }()
	if err := storage.ApplyMigrations(ctx, db.SQL, storage.MigrationsFS(), "migrations"); err != nil {
		t.Fatal(err)
	}

	u, err := storage.CreateUser(ctx, db.SQL, "op@example.com", "x", "operator")
	if err != nil {
		t.Fatal(err)
	}
	sess, err := storage.CreateSession(ctx, db.SQL, u.ID, 30*time.Minute, nil, nil)
	if err != nil {
		t.Fatal(err)
	}

	// Seed minimal domain rows
	tgt, _ := storage.CreateWantedTarget(ctx, db.SQL, "movie", "tmdb", "1", map[string]any{"query": "x"})
	run, _ := storage.CreateSearchRun(ctx, db.SQL, tgt.ID)
	norm := "{}"
	cand, _ := storage.InsertReleaseCandidate(ctx, db.SQL, run.ID, "ix", `{"title":"x"}`, &norm)
	acq, err := storage.CreateAcquisition(ctx, db.SQL, cand.ID, tgt.ID, "torrent", "qbittorrent", "default")
	if err != nil {
		t.Fatal(err)
	}

	app := fiber.New()
	auth := middleware.NewAuth(db.SQL, "sid")
	app.Use(auth.LoadSession())
	api := app.Group("/api/v1")
	RegisterAcquisitions(api, AcquisitionsDeps{DB: db.SQL})

	req := httptest.NewRequest(http.MethodGet, "/api/v1/acquisitions/"+acq.ID, nil)
	req.AddCookie(&http.Cookie{Name: "sid", Value: sess.ID})
	res, err := app.Test(req)
	if err != nil {
		t.Fatal(err)
	}
	if res.StatusCode != 200 {
		t.Fatalf("expected 200, got %d", res.StatusCode)
	}
	var got map[string]any
	_ = json.NewDecoder(res.Body).Decode(&got)
	if got["ID"] != acq.ID {
		t.Fatalf("expected id")
	}
}
