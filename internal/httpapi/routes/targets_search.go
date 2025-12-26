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

		if deps.Hub != nil {
			deps.Hub.Publish("test.target.creating", map[string]any{
				"kind":           req.Kind,
				"externalSource": req.ExternalSource,
				"externalId":     req.ExternalID,
				"query":          q,
			})
		}

		payload := map[string]any{"query": q}
		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()
		t, err := storage.CreateWantedTarget(ctx, deps.DB, req.Kind, req.ExternalSource, req.ExternalID, payload)
		if err != nil {
			if deps.Hub != nil {
				deps.Hub.Publish("test.target.error", map[string]any{
					"error":   "create_failed",
					"message": err.Error(),
				})
			}
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "create_failed"})
		}

		if deps.Hub != nil {
			deps.Hub.Publish("test.target.created", map[string]any{
				"targetId":       t.ID,
				"kind":           req.Kind,
				"externalSource": req.ExternalSource,
				"externalId":     req.ExternalID,
				"query":          q,
			})
		}
		return c.Status(http.StatusCreated).JSON(CreateTargetResponse{ID: t.ID})
	})

	api.Post("/targets/:id/search", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		targetID := strings.TrimSpace(c.Params("id"))
		if targetID == "" {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "missing_target_id"})
		}

		if deps.Hub != nil {
			deps.Hub.Publish("test.search.starting", map[string]any{
				"targetId": targetID,
			})
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()
		run, err := storage.CreateSearchRun(ctx, deps.DB, targetID)
		if err != nil {
			if deps.Hub != nil {
				deps.Hub.Publish("test.search.error", map[string]any{
					"targetId": targetID,
					"error":    "create_run_failed",
					"message":  err.Error(),
				})
			}
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "create_run_failed"})
		}

		job, err := queue.Enqueue(ctx, deps.DB, "search_run", map[string]string{"runId": run.ID, "targetId": targetID}, queue.EnqueueOpts{})
		if err != nil {
			if deps.Hub != nil {
				deps.Hub.Publish("test.search.error", map[string]any{
					"targetId": targetID,
					"runId":    run.ID,
					"error":    "enqueue_failed",
					"message":  err.Error(),
				})
			}
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "enqueue_failed"})
		}
		if deps.Metrics != nil {
			deps.Metrics.JobsEnqueued.Add(1)
		}
		if deps.Hub != nil {
			deps.Hub.Publish("job.enqueued", map[string]any{"jobId": job.ID, "kind": job.Kind, "runId": run.ID, "targetId": targetID})
			deps.Hub.Publish("test.search.started", map[string]any{
				"targetId": targetID,
				"runId":    run.ID,
				"jobId":    job.ID,
			})
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

	api.Post("/candidates/:id/acquisition", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		candidateID := strings.TrimSpace(c.Params("id"))
		if candidateID == "" {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "missing_candidate_id"})
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 8*time.Second)
		defer cancel()

		// Check if acquisition already exists
		existing, found, err := storage.GetAcquisitionByCandidate(ctx, deps.DB, candidateID)
		if err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
		}
		if found {
			// Return existing acquisition
			return c.JSON(fiber.Map{"ok": true, "acquisitionId": existing.ID, "existing": true})
		}

		// Get candidate to find search run and target
		row := deps.DB.QueryRowContext(ctx, `SELECT search_run_id FROM release_candidates WHERE id = ?`, candidateID)
		var searchRunID string
		if err := row.Scan(&searchRunID); err != nil {
			if err == sql.ErrNoRows {
				return c.Status(http.StatusNotFound).JSON(fiber.Map{"error": "candidate_not_found"})
			}
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
		}

		// Get target ID from search run
		row = deps.DB.QueryRowContext(ctx, `SELECT target_id FROM search_runs WHERE id = ?`, searchRunID)
		var targetID string
		if err := row.Scan(&targetID); err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "target_not_found"})
		}

		// Create acquisition (using qbittorrent as default for now, matching search_run.go behavior)
		if deps.Hub != nil {
			deps.Hub.Publish("test.acquisition.creating", map[string]any{
				"candidateId": candidateID,
				"targetId":    targetID,
			})
		}
		acq, err := storage.CreateAcquisition(ctx, deps.DB, candidateID, targetID, "torrent", "qbittorrent", "default")
		if err != nil {
			if deps.Hub != nil {
				deps.Hub.Publish("test.acquisition.error", map[string]any{
					"candidateId": candidateID,
					"targetId":    targetID,
					"error":       "create_failed",
					"message":     err.Error(),
				})
			}
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "create_failed"})
		}

		// Enqueue acquisition job
		job, err := queue.Enqueue(ctx, deps.DB, "acquisition_enqueue", map[string]string{"acquisitionId": acq.ID}, queue.EnqueueOpts{})
		if err != nil {
			// Acquisition is created, but job enqueue failed - still return success with warning
			if deps.Hub != nil {
				deps.Hub.Publish("acquisition.created", map[string]any{"acquisitionId": acq.ID, "candidateId": candidateID, "targetId": targetID, "enqueueFailed": true})
				deps.Hub.Publish("test.acquisition.created", map[string]any{
					"acquisitionId": acq.ID,
					"candidateId":   candidateID,
					"targetId":      targetID,
					"warning":       "enqueue_failed",
				})
			}
			return c.JSON(fiber.Map{"ok": true, "acquisitionId": acq.ID, "jobId": "", "warning": "enqueue_failed"})
		}

		if deps.Metrics != nil {
			deps.Metrics.JobsEnqueued.Add(1)
		}
		if deps.Hub != nil {
			deps.Hub.Publish("job.enqueued", map[string]any{"jobId": job.ID, "kind": "acquisition_enqueue", "acquisitionId": acq.ID, "candidateId": candidateID})
			deps.Hub.Publish("acquisition.created", map[string]any{"acquisitionId": acq.ID, "candidateId": candidateID, "targetId": targetID})
			deps.Hub.Publish("test.acquisition.created", map[string]any{
				"acquisitionId": acq.ID,
				"candidateId":   candidateID,
				"targetId":      targetID,
				"jobId":         job.ID,
			})
		}

		return c.Status(http.StatusCreated).JSON(fiber.Map{"ok": true, "acquisitionId": acq.ID, "jobId": job.ID})
	})
}
