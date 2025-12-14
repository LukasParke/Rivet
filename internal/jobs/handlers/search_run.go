package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"strings"

	"github.com/LukeHagar/Rivet/internal/adapters/indexers/newznab"
	"github.com/LukeHagar/Rivet/internal/decisioning"
	"github.com/LukeHagar/Rivet/internal/jobs/queue"
	"github.com/LukeHagar/Rivet/internal/observability"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/ws"
)

type SearchRunDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
	Hub       *ws.Hub
	Metrics   *observability.Metrics
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
	if deps.Hub != nil {
		deps.Hub.Publish("search.run.started", map[string]any{"runId": runID, "targetId": targetID})
	}
	if deps.Metrics != nil {
		deps.Metrics.SearchRunsStarted.Add(1)
	}
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
		if deps.Hub != nil {
			deps.Hub.Publish("search.run.failed", map[string]any{"runId": runID, "targetId": targetID, "error": e})
		}
		if deps.Metrics != nil {
			deps.Metrics.SearchRunsFailed.Add(1)
		}
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
			if deps.Hub != nil {
				deps.Hub.Publish("candidate.recorded", map[string]any{"runId": runID, "candidateId": cand.ID, "indexer": ix.Name})
			}
			if deps.Metrics != nil {
				deps.Metrics.CandidatesRecorded.Add(1)
			}

			dec := decisioning.Decide(n, policies)
			_, _ = storage.InsertDecision(ctx, deps.DB, cand.ID, dec.Accepted, dec.Score, dec.Reasons)
			if deps.Hub != nil {
				deps.Hub.Publish("decision.recorded", map[string]any{"runId": runID, "candidateId": cand.ID, "accepted": dec.Accepted, "score": dec.Score, "reasons": dec.Reasons})
			}
			if deps.Metrics != nil {
				deps.Metrics.DecisionsRecorded.Add(1)
			}

			// Acquisition v0: for accepted decisions, create acquisition and enqueue to client.
			if dec.Accepted {
				// qBittorrent only for now (torrent).
				a, err := storage.CreateAcquisition(ctx, deps.DB, cand.ID, targetID, "torrent", "qbittorrent", "default")
				if err == nil {
					// Enqueue acquisition job (durable).
					_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_enqueue", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{})
					if deps.Hub != nil {
						deps.Hub.Publish("job.enqueued", map[string]any{"kind": "acquisition_enqueue", "acquisitionId": a.ID, "candidateId": cand.ID})
					}
					if deps.Metrics != nil {
						deps.Metrics.JobsEnqueued.Add(1)
					}
				}
			}
		}
	}

	_ = storage.UpdateSearchRunCompleted(ctx, deps.DB, runID, nil)
	if deps.Hub != nil {
		deps.Hub.Publish("search.run.completed", map[string]any{"runId": runID, "targetId": targetID})
	}
	if deps.Metrics != nil {
		deps.Metrics.SearchRunsCompleted.Add(1)
	}
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
