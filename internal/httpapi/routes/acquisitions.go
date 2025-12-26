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

	"github.com/LukeHagar/Rivet/internal/adapters/downloaders/dryrun"
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

	DryRun            bool
	DryRunFixturePath string
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
	api.Post("/acquisitions/:id/actions/accept_verified", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		return handleAcqAcceptVerified(c, deps)
	})
	api.Post("/acquisitions/:id/actions/override_imported", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		return handleAcqOverrideImported(c, deps)
	})
	api.Post("/acquisitions/:id/actions/blacklist", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		return handleAcqBlacklist(c, deps, false)
	})
	api.Post("/acquisitions/:id/actions/blacklist_and_rescout", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		return handleAcqBlacklist(c, deps, true)
	})
}

type acceptVerifiedRequest struct {
	VerifiedPath string `json:"verifiedPath"`
}

func handleAcqAcceptVerified(c *fiber.Ctx, deps AcquisitionsDeps) error {
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

	var req acceptVerifiedRequest
	_ = c.BodyParser(&req)
	req.VerifiedPath = strings.TrimSpace(req.VerifiedPath)

	// Default to best available local path (admin should know what they're doing).
	verifiedPath := req.VerifiedPath
	if verifiedPath == "" && a.QuarantinedPath != nil {
		verifiedPath = strings.TrimSpace(*a.QuarantinedPath)
	}
	if verifiedPath == "" && a.VerifiedPath != nil {
		verifiedPath = strings.TrimSpace(*a.VerifiedPath)
	}
	if verifiedPath == "" && a.ContentPath != nil {
		verifiedPath = strings.TrimSpace(*a.ContentPath)
	}
	if verifiedPath == "" {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "missing_verified_path"})
	}

	if err := storage.AdminAcceptAcquisitionVerified(ctx, deps.DB, a.ID, verifiedPath); err != nil {
		return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "update_failed"})
	}

	// Enqueue import immediately.
	job, _ := queue.Enqueue(ctx, deps.DB, "acquisition_import", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(1 * time.Second)})

	actor := actorUserIDPtr(c)
	_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{
		ActorUserID: actor,
		Action:      "acquisition.accept_verified",
		TargetType:  strPtr("acquisition"),
		TargetID:    &a.ID,
		DetailsJSON: strPtr(mustJSON(map[string]any{"verifiedPath": verifiedPath, "jobId": job.ID})),
	})
	emitAudit(deps.Hub, "acquisition.accept_verified", actor, map[string]any{"acquisitionId": a.ID, "verifiedPath": verifiedPath})
	if deps.Hub != nil {
		deps.Hub.Publish("verify.completed", map[string]any{"acquisitionId": a.ID, "verifiedPath": verifiedPath, "overridden": true})
		deps.Hub.Publish("job.enqueued", map[string]any{"jobId": job.ID, "kind": job.Kind, "acquisitionId": a.ID})
	}

	return c.JSON(fiber.Map{"ok": true, "jobId": job.ID})
}

type blacklistRequest struct {
	Scope  string `json:"scope"`  // "target" (default) or "global"
	Reason string `json:"reason"` // optional
}

func handleAcqBlacklist(c *fiber.Ctx, deps AcquisitionsDeps, alsoRescout bool) error {
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

	var req blacklistRequest
	_ = c.BodyParser(&req)
	scope := strings.ToLower(strings.TrimSpace(req.Scope))
	if scope == "" {
		scope = "target"
	}
	reason := strings.TrimSpace(req.Reason)
	var reasonPtr *string
	if reason != "" {
		reasonPtr = &reason
	}

	// Resolve candidate URL/title for extra matching.
	candURL, candTitle := loadCandidateURLAndTitle(ctx, deps.DB, a.CandidateID)

	hash := ""
	if a.ExternalID != nil {
		hash = strings.TrimSpace(*a.ExternalID)
	}
	if hash == "" && candURL != "" {
		hash = extractBTIH(candURL)
	}

	var targetPtr *string
	if scope == "target" && strings.TrimSpace(a.TargetID) != "" {
		t := strings.TrimSpace(a.TargetID)
		targetPtr = &t
	}

	created := 0
	matches := []map[string]any{}

	// Prefer hash blacklist (most stable).
	if hash != "" {
		_, err := storage.InsertBlacklistEntry(ctx, deps.DB, storage.BlacklistEntry{
			Scope:      scope,
			TargetID:   targetPtr,
			MatchType:  "hash",
			MatchValue: hash,
			Reason:     reasonPtr,
			CreatedByUserID: actorUserIDPtr(c),
		})
		if err == nil {
			created++
			matches = append(matches, map[string]any{"type": "hash", "value": hash})
		}
	}
	if candURL != "" {
		_, err := storage.InsertBlacklistEntry(ctx, deps.DB, storage.BlacklistEntry{
			Scope:      scope,
			TargetID:   targetPtr,
			MatchType:  "url",
			MatchValue: candURL,
			Reason:     reasonPtr,
			CreatedByUserID: actorUserIDPtr(c),
		})
		if err == nil {
			created++
			matches = append(matches, map[string]any{"type": "url", "value": candURL})
		}
	}
	if candTitle != "" {
		// Title blacklist is optional/weak; still useful as a last resort.
		_, err := storage.InsertBlacklistEntry(ctx, deps.DB, storage.BlacklistEntry{
			Scope:      scope,
			TargetID:   targetPtr,
			MatchType:  "title",
			MatchValue: candTitle,
			Reason:     reasonPtr,
			CreatedByUserID: actorUserIDPtr(c),
		})
		if err == nil {
			created++
			matches = append(matches, map[string]any{"type": "title", "value": candTitle})
		}
	}
	if created == 0 {
		return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "no_match_to_blacklist"})
	}

	action := "acquisition.blacklist"
	if alsoRescout {
		action = "acquisition.blacklist_and_rescout"
	}
	_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{
		ActorUserID: actorUserIDPtr(c),
		Action:      action,
		TargetType:  strPtr("acquisition"),
		TargetID:    &a.ID,
		DetailsJSON: strPtr(mustJSON(map[string]any{"scope": scope, "reason": reason, "matches": matches, "targetId": a.TargetID})),
	})
	emitAudit(deps.Hub, action, actorUserIDPtr(c), map[string]any{"acquisitionId": a.ID, "scope": scope})
	if deps.Hub != nil {
		deps.Hub.Publish("download.blacklisted", map[string]any{"acquisitionId": a.ID, "scope": scope, "matches": matches})
	}

	var jobID *string
	if alsoRescout {
		job, _ := queue.Enqueue(ctx, deps.DB, "acquisition_rescout", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(1 * time.Second)})
		if job.ID != "" {
			jobID = &job.ID
		}
		if deps.Hub != nil {
			deps.Hub.Publish("download.rescout.scheduled", map[string]any{"acquisitionId": a.ID, "jobId": jobID})
		}
	}

	return c.JSON(fiber.Map{"ok": true, "created": created, "jobId": jobID})
}

func actorUserIDPtr(c *fiber.Ctx) *string {
	u, _ := middleware.UserFromContext(c)
	if u.ID == "" {
		return nil
	}
	id := u.ID
	return &id
}

func loadCandidateURLAndTitle(ctx context.Context, db *sql.DB, candidateID string) (string, string) {
	candidateID = strings.TrimSpace(candidateID)
	if candidateID == "" {
		return "", ""
	}
	row := db.QueryRowContext(ctx, `SELECT raw_json FROM release_candidates WHERE id = ?`, candidateID)
	var rawJSON string
	if err := row.Scan(&rawJSON); err != nil {
		return "", ""
	}
	var raw struct {
		EnclosureURL string `json:"enclosureUrl"`
		Link         string `json:"link"`
		Title        string `json:"title"`
	}
	_ = json.Unmarshal([]byte(rawJSON), &raw)
	u := strings.TrimSpace(raw.EnclosureURL)
	if u == "" {
		u = strings.TrimSpace(raw.Link)
	}
	return u, strings.TrimSpace(raw.Title)
}

func extractBTIH(u string) string {
	u = strings.TrimSpace(u)
	if !strings.HasPrefix(strings.ToLower(u), "magnet:?") {
		return ""
	}
	lower := strings.ToLower(u)
	idx := strings.Index(lower, "xt=urn:btih:")
	if idx == -1 {
		return ""
	}
	start := idx + len("xt=urn:btih:")
	end := start
	for end < len(u) {
		ch := u[end]
		if ch == '&' || ch == '#' {
			break
		}
		end++
	}
	h := u[start:end]
	return strings.TrimSpace(h)
}

func mustJSON(v any) string {
	b, _ := json.Marshal(v)
	return string(b)
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

	var cli *qbittorrent.Client
	var dry *dryrun.Client
	if deps.DryRun {
		dry = dryrun.Default(deps.DryRunFixturePath)
	} else {
		cfg, err := loadQBittorrentConfig(ctx, deps)
		if err != nil {
			if fe, ok := err.(*fiber.Error); ok {
				return c.Status(fe.Code).JSON(fiber.Map{"error": fe.Message})
			}
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "client_config_error"})
		}
		c2, err := qbittorrent.New(cfg, deps.Policy)
		if err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "client_init_failed"})
		}
		cli = c2
	}

	switch action {
	case "pause":
		var err error
		if deps.DryRun {
			err = dry.Pause(ctx, *a.ExternalID)
		} else {
			err = cli.Pause(ctx, *a.ExternalID)
		}
		if err != nil {
			return c.Status(http.StatusBadGateway).JSON(fiber.Map{"error": err.Error()})
		}
		_ = storage.UpdateAcquisitionProgress(ctx, deps.DB, a.ID, "paused", a.Progress, a.ContentPath)
	case "resume":
		var err error
		if deps.DryRun {
			err = dry.Resume(ctx, *a.ExternalID)
		} else {
			err = cli.Resume(ctx, *a.ExternalID)
		}
		if err != nil {
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

	a, getErr := storage.GetAcquisition(ctx, deps.DB, id)
	if getErr != nil {
		if getErr == sql.ErrNoRows {
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

	var opErr error
	if deps.DryRun {
		dry := dryrun.Default(deps.DryRunFixturePath)
		opErr = dry.Delete(ctx, *a.ExternalID, del)
	} else {
		cfg, err2 := loadQBittorrentConfig(ctx, deps)
		if err2 != nil {
			if fe, ok := err2.(*fiber.Error); ok {
				return c.Status(fe.Code).JSON(fiber.Map{"error": fe.Message})
			}
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "client_config_error"})
		}
		cli, err2 := qbittorrent.New(cfg, deps.Policy)
		if err2 != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "client_init_failed"})
		}
		opErr = cli.Delete(ctx, *a.ExternalID, del)
	}
	if opErr != nil {
		return c.Status(http.StatusBadGateway).JSON(fiber.Map{"error": opErr.Error()})
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
