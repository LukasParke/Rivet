package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"strings"
	"time"

	"github.com/LukeHagar/Rivet/internal/adapters/indexers/newznab"
	"github.com/LukeHagar/Rivet/internal/decisioning"
	"github.com/LukeHagar/Rivet/internal/jobs/queue"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type RescoutDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
	Hub       publisher
}

// RescoutAcquisition searches for alternatives and enqueues a replacement acquisition.
// Minimal v1: create a new search_run for traceability, record candidates/decisions,
// then create ONE acquisition for the best accepted candidate excluding the current btih/url.
func RescoutAcquisition(ctx context.Context, deps RescoutDeps, acquisitionID string) error {
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	a, err := storage.GetAcquisition(ctx, deps.DB, acquisitionID)
	if err != nil {
		return err
	}
	if a.TargetID == "" {
		return errors.New("missing_target_id")
	}

	excludeCandidateID := strings.TrimSpace(a.CandidateID)
	excludeHash := ""
	if a.ExternalID != nil {
		excludeHash = strings.TrimSpace(*a.ExternalID)
	}
	excludeURL := loadCandidateURL(ctx, deps.DB, excludeCandidateID)
	if excludeHash == "" && excludeURL != "" {
		excludeHash = extractBTIH(excludeURL)
	}

	if deps.Hub != nil {
		deps.Hub.Publish("download.rescout.started", map[string]any{
			"acquisitionId": a.ID,
			"targetId":      a.TargetID,
			"excludeHash":   excludeHash,
		})
	}

	// Create a search run for evidence/debugging.
	run, err := storage.CreateSearchRun(ctx, deps.DB, a.TargetID)
	if err != nil {
		return err
	}
	_ = storage.UpdateSearchRunRunning(ctx, deps.DB, run.ID)

	query := loadTargetQuery(ctx, deps.DB, a.TargetID)

	// Policies
	policies := decisioning.Policies{}
	if s, ok, err := storage.GetSetting(ctx, deps.DB, "setup.policies"); err == nil && ok {
		if p, err := decisioning.ParsePolicies(s); err == nil {
			policies = p
		}
	}

	// Indexers
	indexers, err := loadIndexersFromSettings(ctx, deps.DB, deps.SecretBox)
	if err != nil {
		e := err.Error()
		_ = storage.UpdateSearchRunCompleted(ctx, deps.DB, run.ID, &e)
		if deps.Hub != nil {
			deps.Hub.Publish("download.rescout.failed", map[string]any{"acquisitionId": a.ID, "runId": run.ID, "error": e})
		}
		return err
	}

	bestCandidateID := ""
	bestScore := -1

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

			cand, err := storage.InsertReleaseCandidate(ctx, deps.DB, run.ID, ix.Name, string(rawB), &normS)
			if err != nil {
				continue
			}

			dec := decisioning.Decide(n, policies)
			_, _ = storage.InsertDecision(ctx, deps.DB, cand.ID, dec.Accepted, dec.Score, dec.Reasons)

			if !dec.Accepted {
				continue
			}

			urlToUse := strings.TrimSpace(it.EnclosureURL)
			if urlToUse == "" {
				urlToUse = strings.TrimSpace(it.Link)
			}
			if urlToUse == "" {
				continue
			}

			hash := extractBTIH(urlToUse)
			if excludeHash != "" && hash != "" && strings.EqualFold(hash, excludeHash) {
				continue
			}
			if excludeURL != "" && strings.EqualFold(urlToUse, excludeURL) {
				continue
			}

			// Avoid creating duplicate acquisitions for the same candidate record.
			if _, ok, _ := storage.GetAcquisitionByCandidate(ctx, deps.DB, cand.ID); ok {
				continue
			}

			if dec.Score > bestScore {
				bestScore = dec.Score
				bestCandidateID = cand.ID
			}
		}
	}

	_ = storage.UpdateSearchRunCompleted(ctx, deps.DB, run.ID, nil)

	if bestCandidateID == "" {
		if deps.Hub != nil {
			deps.Hub.Publish("download.rescout.no_alternative", map[string]any{"acquisitionId": a.ID, "runId": run.ID, "targetId": a.TargetID})
		}
		return nil
	}

	newAcq, err := storage.CreateAcquisition(ctx, deps.DB, bestCandidateID, a.TargetID, a.Kind, "qbittorrent", "default")
	if err != nil {
		return err
	}
	_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_enqueue", map[string]string{"acquisitionId": newAcq.ID}, queue.EnqueueOpts{})

	if deps.Hub != nil {
		deps.Hub.Publish("download.rescout.enqueued", map[string]any{
			"acquisitionId":     a.ID,
			"replacementId":     newAcq.ID,
			"runId":             run.ID,
			"replacementCandidateId": bestCandidateID,
		})
	}
	return nil
}

func loadCandidateURL(ctx context.Context, db *sql.DB, candidateID string) string {
	candidateID = strings.TrimSpace(candidateID)
	if candidateID == "" {
		return ""
	}
	row := db.QueryRowContext(ctx, `SELECT raw_json FROM release_candidates WHERE id = ?`, candidateID)
	var rawJSON string
	if err := row.Scan(&rawJSON); err != nil {
		return ""
	}
	var raw struct {
		EnclosureURL string `json:"enclosureUrl"`
		Link         string `json:"link"`
	}
	_ = json.Unmarshal([]byte(rawJSON), &raw)
	u := strings.TrimSpace(raw.EnclosureURL)
	if u == "" {
		u = strings.TrimSpace(raw.Link)
	}
	return u
}
