package routes

import (
	"context"
	"database/sql"
	"encoding/json"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type SetupConfigDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
}

type DecryptedConfigResponse struct {
	Paths    map[string]any `json:"paths,omitempty"`
	Clients  map[string]any `json:"clients,omitempty"`
	Indexers map[string]any `json:"indexers,omitempty"`
	Metadata map[string]any `json:"metadata,omitempty"`
	Policies map[string]any `json:"policies,omitempty"`
}

func RegisterSetupConfig(api fiber.Router, deps SetupConfigDeps) {
	api.Get("/setup/config", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()

		resp := DecryptedConfigResponse{
			Paths:    make(map[string]any),
			Clients:  make(map[string]any),
			Indexers: make(map[string]any),
			Metadata: make(map[string]any),
			Policies: make(map[string]any),
		}

		// Load and decrypt paths
		if v, ok, err := storage.GetSetting(ctx, deps.DB, "setup.paths"); err == nil && ok {
			var paths map[string]any
			if json.Unmarshal([]byte(v), &paths) == nil {
				resp.Paths = paths
			}
		}

		// Load and decrypt clients
		if v, ok, err := storage.GetSetting(ctx, deps.DB, "setup.clients"); err == nil && ok {
			var clients map[string]any
			if json.Unmarshal([]byte(v), &clients) == nil {
				decrypted := make(map[string]any)
				if qb, ok := clients["qbittorrent"].(map[string]any); ok {
					decrypted["qbittorrent"] = map[string]any{
						"url":      qb["url"],
						"username": qb["username"],
					}
					if pwEnc, ok := qb["passwordEnc"].(string); ok && pwEnc != "" && deps.SecretBox != nil {
						if pw, err := deps.SecretBox.DecryptString(pwEnc); err == nil {
							decrypted["qbittorrent"].(map[string]any)["password"] = string(pw)
						}
					}
				}
				resp.Clients = decrypted
			}
		}

		// Load and decrypt indexers
		if v, ok, err := storage.GetSetting(ctx, deps.DB, "setup.indexers"); err == nil && ok {
			var indexers map[string]any
			if json.Unmarshal([]byte(v), &indexers) == nil {
				decrypted := make(map[string]any)
				if torznab, ok := indexers["torznab"].([]any); ok {
					decryptedTorznab := make([]map[string]any, 0, len(torznab))
					for _, ix := range torznab {
						if ixMap, ok := ix.(map[string]any); ok {
							decryptedIx := map[string]any{
								"name":     ixMap["name"],
								"url":      ixMap["url"],
								"username": ixMap["username"],
							}
							if apiKeyEnc, ok := ixMap["apiKeyEnc"].(string); ok && apiKeyEnc != "" && deps.SecretBox != nil {
								if apiKey, err := deps.SecretBox.DecryptString(apiKeyEnc); err == nil {
									decryptedIx["apiKey"] = string(apiKey)
								}
							}
							if pwEnc, ok := ixMap["passwordEnc"].(string); ok && pwEnc != "" && deps.SecretBox != nil {
								if pw, err := deps.SecretBox.DecryptString(pwEnc); err == nil {
									decryptedIx["password"] = string(pw)
								}
							}
							decryptedTorznab = append(decryptedTorznab, decryptedIx)
						}
					}
					decrypted["torznab"] = decryptedTorznab
				}
				resp.Indexers = decrypted
			}
		}

		// Load and decrypt metadata
		if v, ok, err := storage.GetSetting(ctx, deps.DB, "setup.metadata"); err == nil && ok {
			var metadata map[string]any
			if json.Unmarshal([]byte(v), &metadata) == nil {
				decrypted := make(map[string]any)
				if tmdb, ok := metadata["tmdb"].(map[string]any); ok {
					decrypted["tmdb"] = map[string]any{
						"baseUrl": tmdb["baseUrl"],
					}
					if keyEnc, ok := tmdb["apiKeyEnc"].(string); ok && keyEnc != "" && deps.SecretBox != nil {
						if key, err := deps.SecretBox.DecryptString(keyEnc); err == nil {
							decrypted["tmdb"].(map[string]any)["apiKey"] = string(key)
						}
					}
				}
				if tvdb, ok := metadata["tvdb"].(map[string]any); ok {
					decrypted["tvdb"] = map[string]any{
						"baseUrl": tvdb["baseUrl"],
					}
					if keyEnc, ok := tvdb["apiKeyEnc"].(string); ok && keyEnc != "" && deps.SecretBox != nil {
						if key, err := deps.SecretBox.DecryptString(keyEnc); err == nil {
							decrypted["tvdb"].(map[string]any)["apiKey"] = string(key)
						}
					}
				}
				resp.Metadata = decrypted
			}
		}

		// Load policies (no encryption)
		if v, ok, err := storage.GetSetting(ctx, deps.DB, "setup.policies"); err == nil && ok {
			var policies map[string]any
			if json.Unmarshal([]byte(v), &policies) == nil {
				resp.Policies = policies
			}
		}

		return c.JSON(resp)
	})
}

