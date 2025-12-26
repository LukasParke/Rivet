package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"strings"

	"github.com/LukeHagar/Rivet/internal/adapters/indexers/newznab"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

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

func loadTargetQuery(ctx context.Context, db *sql.DB, targetID string) string {
	row := db.QueryRowContext(ctx, `SELECT payload_json FROM wanted_targets WHERE id = ?`, targetID)
	var payload sql.NullString
	if err := row.Scan(&payload); err == nil && payload.Valid {
		var p map[string]any
		if json.Unmarshal([]byte(payload.String), &p) == nil {
			if v, ok := p["query"].(string); ok {
				q := strings.TrimSpace(v)
				if q != "" {
					return q
				}
			}
		}
	}
	return "ubuntu"
}

func loadIndexersFromSettings(ctx context.Context, db *sql.DB, sb *security.SecretBox, hub interface{ Publish(string, any) }, runID, targetID string) ([]newznab.Config, error) {
	if hub != nil {
		hub.Publish("search.indexers.loading", map[string]any{
			"runId":    runID,
			"targetId": targetID,
		})
	}

	if sb == nil {
		if hub != nil {
			hub.Publish("search.indexers.error", map[string]any{
				"runId":    runID,
				"targetId": targetID,
				"error":    "secretbox_missing",
				"step":     "secretbox_check",
			})
		}
		return nil, errors.New("secretbox_missing")
	}

	cfgStr, ok, err := storage.GetSetting(ctx, db, "setup.indexers")
	if hub != nil {
		errorMsg := ""
		if err != nil {
			errorMsg = err.Error()
		}
		preview := ""
		if ok && err == nil {
			preview = cfgStr
			if len(preview) > 200 {
				preview = preview[:200] + "..."
			}
		}
		hub.Publish("search.indexers.settings.retrieved", map[string]any{
			"runId":    runID,
			"targetId": targetID,
			"exists":   ok,
			"error":    errorMsg,
			"preview":  preview,
		})
	}
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, errors.New("no_indexers_configured")
	}

	var p persistedIndexers
	if err := json.Unmarshal([]byte(cfgStr), &p); err != nil {
		if hub != nil {
			hub.Publish("search.indexers.settings.parsed", map[string]any{
				"runId":    runID,
				"targetId": targetID,
				"success":  false,
				"error":    err.Error(),
			})
		}
		return nil, err
	}

	if hub != nil {
		hub.Publish("search.indexers.settings.parsed", map[string]any{
			"runId":      runID,
			"targetId":   targetID,
			"success":    true,
			"torznabCount": len(p.Torznab),
			"newznabCount": len(p.Newznab),
		})
	}

	var out []newznab.Config
	add := func(name, urlStr, keyEnc, user, passEnc string, indexerType string) {
		if hub != nil {
			hub.Publish("search.indexers.decrypting", map[string]any{
				"runId":        runID,
				"targetId":     targetID,
				"indexerName":  name,
				"indexerType":  indexerType,
				"url":          urlStr,
				"hasApiKey":    keyEnc != "",
				"hasUsername":  user != "",
				"hasPassword":  passEnc != "",
			})
		}

		key, err := sb.DecryptString(keyEnc)
		if err != nil {
			if hub != nil {
				hub.Publish("search.indexers.decrypt.failed", map[string]any{
					"runId":         runID,
					"targetId":      targetID,
					"indexerName":   name,
					"indexerType":   indexerType,
					"error":         err.Error(),
					"field":         "apiKey",
					"encryptedLength": len(keyEnc),
					"encryptedPreview": func() string {
						if len(keyEnc) > 20 {
							return keyEnc[:20] + "..."
						}
						return keyEnc
					}(),
				})
			}
			return
		}
		pass := ""
		if strings.TrimSpace(passEnc) != "" {
			if b, err := sb.DecryptString(passEnc); err == nil {
				pass = string(b)
			} else {
				if hub != nil {
					hub.Publish("search.indexers.decrypt.failed", map[string]any{
						"runId":         runID,
						"targetId":      targetID,
						"indexerName":   name,
						"indexerType":   indexerType,
						"error":         err.Error(),
						"field":         "password",
						"encryptedLength": len(passEnc),
						"encryptedPreview": func() string {
							if len(passEnc) > 20 {
								return passEnc[:20] + "..."
							}
							return passEnc
						}(),
					})
				}
			}
		}
		cfg := newznab.Config{Name: name, BaseURL: urlStr, APIKey: string(key), Username: user, Password: pass}
		out = append(out, cfg)
		if hub != nil {
			hub.Publish("search.indexers.config.created", map[string]any{
				"runId":         runID,
				"targetId":      targetID,
				"indexerName":   name,
				"indexerType":   indexerType,
				"url":           urlStr,
				"hasApiKey":     string(key) != "",
				"apiKeyLength":  len(string(key)),
				"hasUsername":   user != "",
				"hasPassword":   pass != "",
				"passwordLength": len(pass),
			})
		}
	}
	if hub != nil {
		hub.Publish("search.indexers.processing", map[string]any{
			"runId":        runID,
			"targetId":     targetID,
			"torznabCount": len(p.Torznab),
			"newznabCount": len(p.Newznab),
		})
	}
	for i, ix := range p.Torznab {
		if hub != nil {
			hub.Publish("search.indexers.processing.item", map[string]any{
				"runId":         runID,
				"targetId":      targetID,
				"index":         i,
				"indexerName":   ix.Name,
				"indexerType":   "torznab",
				"url":           ix.URL,
				"hasApiKeyEnc":  ix.APIKeyEnc != "",
				"apiKeyEncLength": len(ix.APIKeyEnc),
				"hasUsername":   ix.Username != "",
				"hasPasswordEnc": ix.PasswordEnc != "",
				"passwordEncLength": len(ix.PasswordEnc),
			})
		}
		add(ix.Name, ix.URL, ix.APIKeyEnc, ix.Username, ix.PasswordEnc, "torznab")
	}
	for i, ix := range p.Newznab {
		if hub != nil {
			hub.Publish("search.indexers.processing.item", map[string]any{
				"runId":         runID,
				"targetId":      targetID,
				"index":         i,
				"indexerName":   ix.Name,
				"indexerType":   "newznab",
				"url":           ix.URL,
				"hasApiKeyEnc":  ix.APIKeyEnc != "",
				"apiKeyEncLength": len(ix.APIKeyEnc),
				"hasUsername":   ix.Username != "",
				"hasPasswordEnc": ix.PasswordEnc != "",
				"passwordEncLength": len(ix.PasswordEnc),
			})
		}
		add(ix.Name, ix.URL, ix.APIKeyEnc, ix.Username, ix.PasswordEnc, "newznab")
	}

	if hub != nil {
		indexerNames := make([]string, len(out))
		for i, ix := range out {
			indexerNames[i] = ix.Name
		}
		hub.Publish("search.indexers.loaded.detailed", map[string]any{
			"runId":        runID,
			"targetId":     targetID,
			"count":        len(out),
			"indexerNames": indexerNames,
			"torznabCount": len(p.Torznab),
			"newznabCount": len(p.Newznab),
			"successCount": len(out),
			"failedCount":  len(p.Torznab) + len(p.Newznab) - len(out),
		})
	}

	return out, nil
}

