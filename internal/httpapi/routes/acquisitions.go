package routes

import (
	"context"
	"database/sql"
	"encoding/json"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/adapters/downloaders/qbittorrent"
	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/jobs/queue"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/ws"
)

type AcquisitionsDeps struct {
	DB *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
	Hub       *ws.Hub
}

func RegisterAcquisitions(api fiber.Router, deps AcquisitionsDeps) {
	api.Get("/acquisitions", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()
		limit := 50
		if s := strings.TrimSpace(c.Query("limit")); s != "" {
			if n, err := strconv.Atoi(s); err == nil {
				limit = n
			}
		}
		status := strings.TrimSpace(c.Query("status"))
		var (
			items []storage.Acquisition
			err   error
		)
		if status != "" {
			items, err = storage.ListAcquisitionsByStatus(ctx, deps.DB, status, limit)
		} else {
			items, err = storage.ListAcquisitions(ctx, deps.DB, limit)
		}
		if err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
		}
		return c.JSON(items)
	})

	api.Get("/acquisitions/:id", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()
		id := c.Params("id")
		if id == "" {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_id"})
		}
		item, err := storage.GetAcquisition(ctx, deps.DB, id)
		if err != nil {
			if err == sql.ErrNoRows {
				return c.Status(http.StatusNotFound).JSON(fiber.Map{"error": "not_found"})
			}
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
		}
		return c.JSON(item)
	})

	// Control actions (operator): pause/resume/retry; destructive remove is admin-only.
	api.Post("/acquisitions/:id/actions/pause", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		return handleAcqControl(c, deps, "pause")
	})
	api.Post("/acquisitions/:id/actions/resume", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		return handleAcqControl(c, deps, "resume")
	})
	api.Post("/acquisitions/:id/actions/retry", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		return handleAcqRetry(c, deps)
	})
	api.Post("/acquisitions/:id/actions/remove", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		return handleAcqRemove(c, deps)
	})

	// Review queue actions.
	api.Post("/acquisitions/:id/actions/reverify", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		return handleAcqEnqueueJob(c, deps, "acquisition_verify", "acquisition.reverify")
	})
	api.Post("/acquisitions/:id/actions/reimport", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		return handleAcqEnqueueJob(c, deps, "acquisition_import", "acquisition.reimport")
	})
	api.Post("/acquisitions/:id/actions/override_imported", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		return handleAcqOverrideImported(c, deps)
	})
}

type persistedClients struct {
	QBittorrent *struct {
		URL         string `json:"url"`
		Username    string `json:"username"`
		PasswordEnc string `json:"passwordEnc"`
	} `json:"qbittorrent"`
}

func loadQBittorrentConfig(ctx context.Context, deps AcquisitionsDeps) (qbittorrent.Config, error) {
	cfgStr, ok, err := storage.GetSetting(ctx, deps.DB, "setup.clients")
	if err != nil {
		return qbittorrent.Config{}, err
	}
	if !ok {
		return qbittorrent.Config{}, fiber.NewError(http.StatusBadRequest, "no_clients_configured")
	}
	var p persistedClients
	if err := json.Unmarshal([]byte(cfgStr), &p); err != nil {
		return qbittorrent.Config{}, fiber.NewError(http.StatusBadRequest, "invalid_clients_config")
	}
	if p.QBittorrent == nil {
		return qbittorrent.Config{}, fiber.NewError(http.StatusBadRequest, "qbittorrent_not_configured")
	}
	if deps.SecretBox == nil {
		return qbittorrent.Config{}, fiber.NewError(http.StatusInternalServerError, "secretbox_missing")
	}
	pw, err := deps.SecretBox.DecryptString(p.QBittorrent.PasswordEnc)
	if err != nil {
		return qbittorrent.Config{}, fiber.NewError(http.StatusBadRequest, "decrypt_failed")
	}
	return qbittorrent.Config{BaseURL: p.QBittorrent.URL, Username: p.QBittorrent.Username, Password: string(pw)}, nil
}

func handleAcqControl(c *fiber.Ctx, deps AcquisitionsDeps, action string) error {
	ctx, cancel := context.WithTimeout(c.UserContext(), 10*time.Second)
	defer cancel()
	id := strings.TrimSpace(c.Params("id"))
	if id == "" {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_id"})
	}
	a, err := storage.GetAcquisition(ctx, deps.DB, id)
	if err != nil {
		if err == sql.ErrNoRows {
			return c.Status(http.StatusNotFound).JSON(fiber.Map{"error": "not_found"})
		}
		return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
	}
	if a.ClientKind != "qbittorrent" {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "unsupported_client_kind"})
	}
	if a.ExternalID == nil || strings.TrimSpace(*a.ExternalID) == "" {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "missing_external_id"})
	}

	cfg, err := loadQBittorrentConfig(ctx, deps)
	if err != nil {
		if fe, ok := err.(*fiber.Error); ok {
			return c.Status(fe.Code).JSON(fiber.Map{"error": fe.Message})
		}
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "client_config_error"})
	}
	cli, err := qbittorrent.New(cfg, deps.Policy)
	if err != nil {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "client_init_failed"})
	}

	switch action {
	case "pause":
		if err := cli.Pause(ctx, *a.ExternalID); err != nil {
			return c.Status(http.StatusBadGateway).JSON(fiber.Map{"error": err.Error()})
		}
		_ = storage.UpdateAcquisitionProgress(ctx, deps.DB, a.ID, "paused", a.Progress, a.ContentPath)
	case "resume":
		if err := cli.Resume(ctx, *a.ExternalID); err != nil {
			return c.Status(http.StatusBadGateway).JSON(fiber.Map{"error": err.Error()})
		}
		_ = storage.UpdateAcquisitionProgress(ctx, deps.DB, a.ID, "downloading", a.Progress, a.ContentPath)
		_ = storage.TouchAcquisitionProgressChanged(ctx, deps.DB, a.ID, time.Now().UTC())
	default:
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_action"})
	}

	u, _ := middleware.UserFromContext(c)
	var userIDPtr *string
	if u.ID != "" {
		userIDPtr = &u.ID
	}
	targetType := "acquisition"
	_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{
		ActorUserID: userIDPtr,
		Action:      "acquisition." + action,
		TargetType:  &targetType,
		TargetID:    &a.ID,
	})
	emitAudit(deps.Hub, "acquisition."+action, userIDPtr, map[string]any{"acquisitionId": a.ID})
	if deps.Hub != nil {
		deps.Hub.Publish("download.control", map[string]any{"acquisitionId": a.ID, "action": action})
	}
	return c.JSON(fiber.Map{"ok": true})
}

func handleAcqRemove(c *fiber.Ctx, deps AcquisitionsDeps) error {
	ctx, cancel := context.WithTimeout(c.UserContext(), 10*time.Second)
	defer cancel()
	id := strings.TrimSpace(c.Params("id"))
	if id == "" {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_id"})
	}
	deleteFiles := strings.TrimSpace(c.Query("deleteFiles"))
	del := deleteFiles == "1" || strings.EqualFold(deleteFiles, "true") || strings.EqualFold(deleteFiles, "yes")

	a, err := storage.GetAcquisition(ctx, deps.DB, id)
	if err != nil {
		if err == sql.ErrNoRows {
			return c.Status(http.StatusNotFound).JSON(fiber.Map{"error": "not_found"})
		}
		return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
	}
	if a.ClientKind != "qbittorrent" {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "unsupported_client_kind"})
	}
	if a.ExternalID == nil || strings.TrimSpace(*a.ExternalID) == "" {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "missing_external_id"})
	}

	cfg, err := loadQBittorrentConfig(ctx, deps)
	if err != nil {
		if fe, ok := err.(*fiber.Error); ok {
			return c.Status(fe.Code).JSON(fiber.Map{"error": fe.Message})
		}
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "client_config_error"})
	}
	cli, err := qbittorrent.New(cfg, deps.Policy)
	if err != nil {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "client_init_failed"})
	}
	if err := cli.Delete(ctx, *a.ExternalID, del); err != nil {
		return c.Status(http.StatusBadGateway).JSON(fiber.Map{"error": err.Error()})
	}
	_ = storage.UpdateAcquisitionProgress(ctx, deps.DB, a.ID, "removed", a.Progress, a.ContentPath)

	u, _ := middleware.UserFromContext(c)
	var userIDPtr *string
	if u.ID != "" {
		userIDPtr = &u.ID
	}
	targetType := "acquisition"
	_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{
		ActorUserID: userIDPtr,
		Action:      "acquisition.remove",
		TargetType:  &targetType,
		TargetID:    &a.ID,
		DetailsJSON: strPtr(`{"deleteFiles":` + strconv.FormatBool(del) + `}`),
	})
	emitAudit(deps.Hub, "acquisition.remove", userIDPtr, map[string]any{"acquisitionId": a.ID, "deleteFiles": del})
	if deps.Hub != nil {
		deps.Hub.Publish("download.control", map[string]any{"acquisitionId": a.ID, "action": "remove", "deleteFiles": del})
	}
	return c.JSON(fiber.Map{"ok": true})
}

func handleAcqRetry(c *fiber.Ctx, deps AcquisitionsDeps) error {
	return handleAcqEnqueueJob(c, deps, "acquisition_enqueue", "acquisition.retry")
}

func handleAcqEnqueueJob(c *fiber.Ctx, deps AcquisitionsDeps, jobKind string, auditAction string) error {
	ctx, cancel := context.WithTimeout(c.UserContext(), 8*time.Second)
	defer cancel()
	id := strings.TrimSpace(c.Params("id"))
	if id == "" {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_id"})
	}
	if _, err := storage.GetAcquisition(ctx, deps.DB, id); err != nil {
		if err == sql.ErrNoRows {
			return c.Status(http.StatusNotFound).JSON(fiber.Map{"error": "not_found"})
		}
		return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
	}

	job, err := queue.Enqueue(ctx, deps.DB, jobKind, map[string]string{"acquisitionId": id}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(1 * time.Second)})
	if err != nil {
		return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "enqueue_failed"})
	}
	if deps.Hub != nil {
		deps.Hub.Publish("job.enqueued", map[string]any{"jobId": job.ID, "kind": job.Kind, "acquisitionId": id})
	}

	u, _ := middleware.UserFromContext(c)
	var userIDPtr *string
	if u.ID != "" {
		userIDPtr = &u.ID
	}
	targetType := "acquisition"
	_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{
		ActorUserID: userIDPtr,
		Action:      auditAction,
		TargetType:  &targetType,
		TargetID:    &id,
	})
	emitAudit(deps.Hub, auditAction, userIDPtr, map[string]any{"acquisitionId": id, "jobKind": jobKind})
	return c.JSON(fiber.Map{"ok": true, "jobId": job.ID})
}

type overrideImportedRequest struct {
	ImportedPath string `json:"importedPath"`
}

func handleAcqOverrideImported(c *fiber.Ctx, deps AcquisitionsDeps) error {
	ctx, cancel := context.WithTimeout(c.UserContext(), 8*time.Second)
	defer cancel()
	id := strings.TrimSpace(c.Params("id"))
	if id == "" {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_id"})
	}
	var req overrideImportedRequest
	if err := c.BodyParser(&req); err != nil {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
	}
	req.ImportedPath = strings.TrimSpace(req.ImportedPath)
	if req.ImportedPath == "" {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "missing_imported_path"})
	}
	if _, err := storage.GetAcquisition(ctx, deps.DB, id); err != nil {
		if err == sql.ErrNoRows {
			return c.Status(http.StatusNotFound).JSON(fiber.Map{"error": "not_found"})
		}
		return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
	}
	if err := storage.MarkAcquisitionImported(ctx, deps.DB, id, req.ImportedPath); err != nil {
		return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "update_failed"})
	}
	u, _ := middleware.UserFromContext(c)
	var userIDPtr *string
	if u.ID != "" {
		userIDPtr = &u.ID
	}
	targetType := "acquisition"
	_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{
		ActorUserID: userIDPtr,
		Action:      "acquisition.override_imported",
		TargetType:  &targetType,
		TargetID:    &id,
		DetailsJSON: strPtr(`{"importedPath":` + strconv.Quote(req.ImportedPath) + `}`),
	})
	emitAudit(deps.Hub, "acquisition.override_imported", userIDPtr, map[string]any{"acquisitionId": id, "importedPath": req.ImportedPath})
	if deps.Hub != nil {
		deps.Hub.Publish("import.completed", map[string]any{"acquisitionId": id, "importedPath": req.ImportedPath, "overridden": true})
	}
	return c.JSON(fiber.Map{"ok": true})
}

func strPtr(s string) *string {
	s = strings.TrimSpace(s)
	if s == "" {
		return nil
	}
	return &s
}
