package routes

import (
	"context"
	"database/sql"
	"encoding/json"
	"net/http"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/adapters/metadata/tmdb"
	"github.com/LukeHagar/Rivet/internal/adapters/metadata/tvdb"
	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type SetupMetadataDeps struct {
	DB *sql.DB

	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
}

type TMDBConfig struct {
	BaseURL string `json:"baseUrl"`
	APIKey  string `json:"apiKey"`
}

type TVDBConfig struct {
	BaseURL string `json:"baseUrl"`
	APIKey  string `json:"apiKey"`
}

type SetupMetadataValidateRequest struct {
	TMDB *TMDBConfig `json:"tmdb,omitempty"`
	TVDB *TVDBConfig `json:"tvdb,omitempty"`
}

type ProviderTestResult struct {
	Ok      bool              `json:"ok"`
	Details map[string]string `json:"details,omitempty"`
	Error   string            `json:"error,omitempty"`
}

type SetupMetadataValidateResponse struct {
	Ok bool `json:"ok"`

	TMDB *ProviderTestResult `json:"tmdb,omitempty"`
	TVDB *ProviderTestResult `json:"tvdb,omitempty"`
}

type SetupMetadataSaveResponse struct {
	Saved bool `json:"saved"`
}

func RegisterSetupMetadata(api fiber.Router, deps SetupMetadataDeps) {
	grp := api.Group("/setup")

	grp.Post("/metadata/validate", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupMetadataValidateRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 15*time.Second)
		defer cancel()

		resp := validateMetadata(ctx, deps, req)
		return c.JSON(resp)
	})

	grp.Post("/metadata/save", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupMetadataValidateRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 15*time.Second)
		defer cancel()

		resp := validateMetadata(ctx, deps, req)
		if !resp.Ok {
			return c.Status(http.StatusBadRequest).JSON(resp)
		}

		persist := map[string]any{}
		if req.TMDB != nil {
			keyEnc, _ := deps.SecretBox.EncryptToString([]byte(strings.TrimSpace(req.TMDB.APIKey)))
			persist["tmdb"] = map[string]any{"baseUrl": strings.TrimSpace(req.TMDB.BaseURL), "apiKeyEnc": keyEnc}
		}
		if req.TVDB != nil {
			keyEnc, _ := deps.SecretBox.EncryptToString([]byte(strings.TrimSpace(req.TVDB.APIKey)))
			persist["tvdb"] = map[string]any{"baseUrl": strings.TrimSpace(req.TVDB.BaseURL), "apiKeyEnc": keyEnc}
		}

		b, _ := json.Marshal(persist)
		if err := storage.UpsertSetting(ctx, deps.DB, "setup.metadata", string(b)); err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "save_failed"})
		}
		_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{Action: "setup.metadata_saved"})

		return c.JSON(SetupMetadataSaveResponse{Saved: true})
	})
}

func validateMetadata(ctx context.Context, deps SetupMetadataDeps, req SetupMetadataValidateRequest) SetupMetadataValidateResponse {
	resp := SetupMetadataValidateResponse{Ok: true}
	if req.TMDB == nil && req.TVDB == nil {
		resp.Ok = false
		return resp
	}

	if req.TMDB != nil {
		c, err := tmdb.New(req.TMDB.BaseURL, req.TMDB.APIKey, deps.Policy)
		if err != nil {
			r := ProviderTestResult{Ok: false, Error: err.Error()}
			resp.TMDB = &r
			resp.Ok = false
		} else {
			base, err := c.Validate(ctx)
			if err != nil {
				r := ProviderTestResult{Ok: false, Error: err.Error()}
				resp.TMDB = &r
				resp.Ok = false
			} else {
				r := ProviderTestResult{Ok: true, Details: map[string]string{"imagesBaseUrl": base}}
				resp.TMDB = &r
			}
		}
	}

	if req.TVDB != nil {
		c, err := tvdb.New(req.TVDB.BaseURL, req.TVDB.APIKey, deps.Policy)
		if err != nil {
			r := ProviderTestResult{Ok: false, Error: err.Error()}
			resp.TVDB = &r
			resp.Ok = false
		} else {
			ok, err := c.Validate(ctx)
			if err != nil {
				r := ProviderTestResult{Ok: false, Error: err.Error()}
				resp.TVDB = &r
				resp.Ok = false
			} else {
				r := ProviderTestResult{Ok: true, Details: map[string]string{"status": ok}}
				resp.TVDB = &r
			}
		}
	}

	return resp
}
