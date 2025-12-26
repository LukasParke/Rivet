package handlers

import (
	"context"
	"database/sql"
	"encoding/json"

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
	if deps.Hub != nil {
		deps.Hub.Publish("search.indexers.loading.start", map[string]any{
			"runId":    runID,
			"targetId": targetID,
			"hasSecretBox": deps.SecretBox != nil,
		})
	}
	indexers, err := loadIndexersFromSettings(ctx, deps.DB, deps.SecretBox, deps.Hub, runID, targetID)
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

	// Publish indexer loading event
	if deps.Hub != nil {
		indexerNames := make([]string, len(indexers))
		for i, ix := range indexers {
			indexerNames[i] = ix.Name
		}
		deps.Hub.Publish("search.indexers.loaded", map[string]any{
			"runId":        runID,
			"targetId":     targetID,
			"count":        len(indexers),
			"indexerNames": indexerNames,
		})
	}

	// Publish search loop start event
	if deps.Hub != nil {
		deps.Hub.Publish("search.loop.started", map[string]any{
			"runId":    runID,
			"targetId": targetID,
			"query":    query,
			"count":    len(indexers),
		})
	}

	// Run searches
	for _, ix := range indexers {
		// Publish individual indexer search start event
		if deps.Hub != nil {
			deps.Hub.Publish("search.indexer.started", map[string]any{
				"runId":    runID,
				"targetId": targetID,
				"indexer":  ix.Name,
				"query":    query,
			})
		}
		cli, err := newznab.New(ix, deps.Policy)
		if err != nil {
			if deps.Hub != nil {
				deps.Hub.Publish("search.indexer.error", map[string]any{
					"runId":    runID,
					"targetId": targetID,
					"indexer":  ix.Name,
					"error":    "client_create_failed: " + err.Error(),
				})
			}
			continue
		}

		var debug newznab.DebugInfo
		items, err := cli.SearchWithDebug(ctx, query, 25, &debug)

		// Publish debug info for all searches (successful or failed)
		if deps.Hub != nil {
			if err != nil {
				deps.Hub.Publish("search.indexer.error", map[string]any{
					"runId":      runID,
					"targetId":   targetID,
					"indexer":    ix.Name,
					"query":      query,
					"error":      err.Error(),
					"debug": map[string]any{
						"url":        debug.URL,
						"statusCode": debug.StatusCode,
						"bodyLength": debug.BodyLength,
						"body":       debug.Body,
						"parseError": debug.ParseError,
					},
				})
			} else if len(items) == 0 {
				// Search succeeded but returned no items
				deps.Hub.Publish("search.indexer.empty", map[string]any{
					"runId":      runID,
					"targetId":   targetID,
					"indexer":    ix.Name,
					"query":      query,
					"debug": map[string]any{
						"url":        debug.URL,
						"statusCode": debug.StatusCode,
						"bodyLength": debug.BodyLength,
						"body":       debug.Body,
						"itemCount":  debug.ItemCount,
					},
				})
			} else {
				// Search succeeded with items - publish basic debug info
				deps.Hub.Publish("search.indexer.debug", map[string]any{
					"runId":      runID,
					"targetId":   targetID,
					"indexer":    ix.Name,
					"query":      query,
					"itemCount":  len(items),
					"debug": map[string]any{
						"url":        debug.URL,
						"statusCode": debug.StatusCode,
						"bodyLength": debug.BodyLength,
					},
				})
			}
		}

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
