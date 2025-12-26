package routes

import (
	"context"
	"database/sql"
	"encoding/json"
	"net/http"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/ws"
)

type QualityProfile struct {
	Name             string   `json:"name"`
	AllowedQualities []string `json:"allowedQualities"`
	UpgradeAllowed   bool     `json:"upgradeAllowed"`
}

type SetupPoliciesRequest struct {
	Language        string          `json:"language"`
	BannedTags      []string        `json:"bannedTags"`
	QualityProfiles []QualityProfile `json:"qualityProfiles"`
}

type SetupPoliciesValidateResponse struct {
	Ok     bool     `json:"ok"`
	Errors []string `json:"errors,omitempty"`
}

type SetupPoliciesSaveResponse struct {
	Saved bool `json:"saved"`
}

func RegisterSetupPolicies(api fiber.Router, db *sql.DB, hub *ws.Hub) {
	grp := api.Group("/setup")

	grp.Post("/policies/validate", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupPoliciesRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}
		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()
		_ = ctx
		resp := validatePolicies(hub, req)
		return c.JSON(resp)
	})

	grp.Post("/policies/save", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupPoliciesRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}

		v := validatePolicies(hub, req)
		if !v.Ok {
			return c.Status(http.StatusBadRequest).JSON(v)
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()

		b, _ := json.Marshal(req)
		if err := storage.UpsertSetting(ctx, db, "setup.policies", string(b)); err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "save_failed"})
		}
		var actor *string
		if u, ok := middleware.UserFromContext(c); ok {
			actor = &u.ID
		}
		_ = storage.InsertAuditEvent(ctx, db, storage.AuditEvent{Action: "setup.policies_saved", ActorUserID: actor})
		emitAudit(hub, "setup.policies_saved", actor, nil)
		return c.JSON(SetupPoliciesSaveResponse{Saved: true})
	})
}

func validatePolicies(hub *ws.Hub, req SetupPoliciesRequest) SetupPoliciesValidateResponse {
	if hub != nil {
		hub.Publish("setup.policies.validation.started", map[string]any{
			"qualityProfilesCount": len(req.QualityProfiles),
			"bannedTagsCount":       len(req.BannedTags),
		})
	}

	var errs []string

	lang := strings.TrimSpace(req.Language)
	if lang == "" {
		errs = append(errs, "language is required")
	}

	if len(req.QualityProfiles) == 0 {
		errs = append(errs, "at least one quality profile is required")
	}

	seen := map[string]bool{}
	for i := range req.QualityProfiles {
		p := &req.QualityProfiles[i]
		p.Name = strings.TrimSpace(p.Name)
		if p.Name == "" {
			errs = append(errs, "qualityProfiles.name is required")
			continue
		}
		if seen[strings.ToLower(p.Name)] {
			errs = append(errs, "qualityProfiles.name must be unique")
		}
		seen[strings.ToLower(p.Name)] = true
		if len(p.AllowedQualities) == 0 {
			errs = append(errs, "qualityProfiles.allowedQualities must be non-empty")
		}
	}

	for _, t := range req.BannedTags {
		if strings.TrimSpace(t) == "" {
			errs = append(errs, "bannedTags must not include empty strings")
			break
		}
	}

	if len(errs) > 0 {
		if hub != nil {
			hub.Publish("setup.policies.validation.error", map[string]any{
				"error":   "validation_failed",
				"errors":  errs,
				"message": "Policy validation failed",
			})
		}
		return SetupPoliciesValidateResponse{Ok: false, Errors: errs}
	}

	if hub != nil {
		hub.Publish("setup.policies.validation.completed", map[string]any{
			"ok": true,
			"summary": map[string]any{
				"language":           lang,
				"qualityProfiles":    len(req.QualityProfiles),
				"bannedTags":         len(req.BannedTags),
			},
		})
	}
	return SetupPoliciesValidateResponse{Ok: true}
}
