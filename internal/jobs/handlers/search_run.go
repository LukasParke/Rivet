package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"strings"

	"github.com/LukeHagar/Rivet/internal/adapters/indexers/newznab"
	"github.com/LukeHagar/Rivet/internal/decisioning"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type SearchRunDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
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

func ExecuteSearchRun(ctx context.Context, deps SearchRunDeps, runID, targetID string) error {
	// Mark running
	_ = storage.UpdateSearchRunRunning(ctx, deps.DB, runID)

	// Load target query
	query := loadTargetQuery(ctx, deps.DB, targetID)

	// Load policies
	policies := decisioning.Policies{}
	if s, ok, err := storage.GetSetting(ctx, deps.DB, "setup.policies"); err == nil && ok {
		if p, err := decisioning.ParsePolicies(s); err == nil {
			policies = p
		}
	}

	// Load indexers
	indexers, err := loadIndexers(ctx, deps)
	if err != nil {
		e := err.Error()
		_ = storage.UpdateSearchRunCompleted(ctx, deps.DB, runID, &e)
		return err
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
				"title":        it.Title,
				"guid":         it.GUID,
				"link":         it.Link,
				"enclosureUrl": it.EnclosureURL,
				"pubDate":      it.PubDate,
				"attrs":        it.Attrs,
			}
			rawB, _ := json.Marshal(raw)

			n := decisioning.NormalizeTitle(it.Title)
			normB, _ := json.Marshal(n)
			normS := string(normB)

			cand, err := storage.InsertReleaseCandidate(ctx, deps.DB, runID, ix.Name, string(rawB), &normS)
			if err != nil {
				continue
			}

			dec := decisioning.Decide(n, policies)
			_, _ = storage.InsertDecision(ctx, deps.DB, cand.ID, dec.Accepted, dec.Score, dec.Reasons)
		}
	}

	_ = storage.UpdateSearchRunCompleted(ctx, deps.DB, runID, nil)
	return nil
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

func loadIndexers(ctx context.Context, deps SearchRunDeps) ([]newznab.Config, error) {
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
