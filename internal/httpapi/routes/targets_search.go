package routes

import (
	"context"
	"database/sql"
	"net/http"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/observability"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/jobs/queue"
	"github.com/LukeHagar/Rivet/internal/ws"
)

type TargetsDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
	Hub       *ws.Hub
	Metrics   *observability.Metrics
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

		job, err := queue.Enqueue(ctx, deps.DB, "search_run", map[string]string{"runId": run.ID, "targetId": targetID}, queue.EnqueueOpts{})
		if err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "enqueue_failed"})
		}
		if deps.Metrics != nil {
			deps.Metrics.JobsEnqueued.Add(1)
		}
		if deps.Hub != nil {
			deps.Hub.Publish("job.enqueued", map[string]any{"jobId": job.ID, "kind": job.Kind, "runId": run.ID, "targetId": targetID})
		}

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
