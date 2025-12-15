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

func loadIndexersFromSettings(ctx context.Context, db *sql.DB, sb *security.SecretBox) ([]newznab.Config, error) {
	if sb == nil {
		return nil, errors.New("secretbox_missing")
	}
	cfgStr, ok, err := storage.GetSetting(ctx, db, "setup.indexers")
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
		key, err := sb.DecryptString(keyEnc)
		if err != nil {
			return
		}
		pass := ""
		if strings.TrimSpace(passEnc) != "" {
			if b, err := sb.DecryptString(passEnc); err == nil {
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

