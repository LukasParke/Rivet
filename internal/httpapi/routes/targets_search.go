package routes

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"net/http"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/adapters/indexers/newznab"
	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/jobs"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type TargetsDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
}

type CreateTargetRequest struct {
	Kind           string `json:"kind"`
	ExternalSource string `json:"externalSource"`
	ExternalID     string `json:"externalId"`
	Query          string `json:"query"`
}

type CreateTargetResponse struct {
	ID string `json:"id"`
}

type StartSearchResponse struct {
	RunID string `json:"runId"`
}

type SearchRunResponse struct {
	Run        storage.SearchRun                 `json:"run"`
	Candidates []storage.CandidateWithDecision   `json:"candidates"`
}

func RegisterTargetsAndSearch(api fiber.Router, deps TargetsDeps) {
	api.Get("/targets", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()
		items, err := storage.ListWantedTargets(ctx, deps.DB, 50)
		if err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
		}
		return c.JSON(items)
	})

	api.Post("/targets", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		var req CreateTargetRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}
		req.Kind = strings.TrimSpace(req.Kind)
		req.ExternalSource = strings.TrimSpace(req.ExternalSource)
		req.ExternalID = strings.TrimSpace(req.ExternalID)
		q := strings.TrimSpace(req.Query)
		if req.Kind == "" || req.ExternalSource == "" || req.ExternalID == "" || q == "" {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "missing_fields"})
		}

		payload := map[string]any{"query": q}
		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()
		t, err := storage.CreateWantedTarget(ctx, deps.DB, req.Kind, req.ExternalSource, req.ExternalID, payload)
		if err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "create_failed"})
		}
		return c.Status(http.StatusCreated).JSON(CreateTargetResponse{ID: t.ID})
	})

	api.Post("/targets/:id/search", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		targetID := strings.TrimSpace(c.Params("id"))
		if targetID == "" {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "missing_target_id"})
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()
		run, err := storage.CreateSearchRun(ctx, deps.DB, targetID)
		if err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "create_run_failed"})
		}

		// Kick off background execution (best-effort). Durable jobs come next.
		go executeSearchRun(context.Background(), deps, run.ID, targetID)

		return c.Status(http.StatusAccepted).JSON(StartSearchResponse{RunID: run.ID})
	})

	api.Get("/search/runs/:id", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		runID := strings.TrimSpace(c.Params("id"))
		ctx, cancel := context.WithTimeout(c.UserContext(), 8*time.Second)
		defer cancel()
		run, results, err := storage.GetSearchRunWithResults(ctx, deps.DB, runID)
		if err != nil {
			return c.Status(http.StatusNotFound).JSON(fiber.Map{"error": "not_found"})
		}
		return c.JSON(SearchRunResponse{Run: run, Candidates: results})
	})
}

func executeSearchRun(ctx context.Context, deps TargetsDeps, runID, targetID string) {
	// Mark running
	_ = storage.UpdateSearchRunRunning(ctx, deps.DB, runID)

	// Load target query
	var query string
	{
		row := deps.DB.QueryRowContext(ctx, `SELECT payload_json FROM wanted_targets WHERE id = ?`, targetID)
		var payload sql.NullString
		if err := row.Scan(&payload); err == nil && payload.Valid {
			var p map[string]any
			if json.Unmarshal([]byte(payload.String), &p) == nil {
				if v, ok := p["query"].(string); ok {
					query = strings.TrimSpace(v)
				}
			}
		}
		if query == "" {
			query = "ubuntu"
		}
	}

	// Load policies
	policies := jobs.Policies{}
	{
		if s, ok, err := storage.GetSetting(ctx, deps.DB, "setup.policies"); err == nil && ok {
			if p, err := jobs.ParsePolicies(s); err == nil {
				policies = p
			}
		}
	}

	// Load indexers (encrypted)
	indexers, err := loadIndexers(ctx, deps)
	if err != nil {
		e := err.Error()
		_ = storage.UpdateSearchRunCompleted(ctx, deps.DB, runID, &e)
		return
	}

	// Run searches
	for _, ix := range indexers {
		cli, err := newznab.New(ix, deps.Policy)
		if err != nil {
			continue
		}
		items, err := cli.Search(ctx, query, 25)
		if err != nil {
			continue
		}

		for _, it := range items {
			raw := map[string]any{
				"title": it.Title,
				"guid":  it.GUID,
				"link":  it.Link,
				"enclosureUrl": it.EnclosureURL,
				"pubDate": it.PubDate,
				"attrs": it.Attrs,
			}
			rawB, _ := json.Marshal(raw)

			n := jobs.NormalizeTitle(it.Title)
			normB, _ := json.Marshal(n)
			normS := string(normB)

			cand, err := storage.InsertReleaseCandidate(ctx, deps.DB, runID, ix.Name, string(rawB), &normS)
			if err != nil {
				continue
			}

			dec := jobs.Decide(n, policies)
			_, _ = storage.InsertDecision(ctx, deps.DB, cand.ID, dec.Accepted, dec.Score, dec.Reasons)
		}
	}

	_ = storage.UpdateSearchRunCompleted(ctx, deps.DB, runID, nil)
}

type persistedIndexers struct {
	Torznab []struct {
		Name        string `json:"name"`
		URL         string `json:"url"`
		APIKeyEnc   string `json:"apiKeyEnc"`
		Username    string `json:"username"`
		PasswordEnc string `json:"passwordEnc"`
	} `json:"torznab"`
	Newznab []struct {
		Name        string `json:"name"`
		URL         string `json:"url"`
		APIKeyEnc   string `json:"apiKeyEnc"`
		Username    string `json:"username"`
		PasswordEnc string `json:"passwordEnc"`
	} `json:"newznab"`
}

func loadIndexers(ctx context.Context, deps TargetsDeps) ([]newznab.Config, error) {
	cfgStr, ok, err := storage.GetSetting(ctx, deps.DB, "setup.indexers")
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, errors.New("no_indexers_configured")
	}
	var p persistedIndexers
	if err := json.Unmarshal([]byte(cfgStr), &p); err != nil {
		return nil, err
	}

	var out []newznab.Config
	add := func(name, urlStr, keyEnc, user, passEnc string) {
		key, err := deps.SecretBox.DecryptString(keyEnc)
		if err != nil {
			return
		}
		pass := ""
		if strings.TrimSpace(passEnc) != "" {
			if b, err := deps.SecretBox.DecryptString(passEnc); err == nil {
				pass = string(b)
			}
		}
		out = append(out, newznab.Config{Name: name, BaseURL: urlStr, APIKey: string(key), Username: user, Password: pass})
	}
	for _, ix := range p.Torznab {
		add(ix.Name, ix.URL, ix.APIKeyEnc, ix.Username, ix.PasswordEnc)
	}
	for _, ix := range p.Newznab {
		add(ix.Name, ix.URL, ix.APIKeyEnc, ix.Username, ix.PasswordEnc)
	}
	return out, nil
}
