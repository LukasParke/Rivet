package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"path/filepath"
	"strings"
	"time"

	"github.com/LukeHagar/Rivet/internal/adapters/downloaders/qbittorrent"
	"github.com/LukeHagar/Rivet/internal/jobs/queue"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type MonitorDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
	PollEvery time.Duration
	Hub       publisher
	StallAfter time.Duration
}

type setupPaths struct {
	DownloadsCompletedPath string   `json:"downloadsCompletedPath"`
	LibraryRoots           []string `json:"libraryRoots"`
	QuarantinePath         string   `json:"quarantinePath"`
}

func MonitorAcquisition(ctx context.Context, deps MonitorDeps, acquisitionID string) error {
	if deps.PollEvery <= 0 {
		deps.PollEvery = 10 * time.Second
	}
	if deps.StallAfter <= 0 {
		deps.StallAfter = 30 * time.Minute
	}
	ctx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()

	a, err := storage.GetAcquisition(ctx, deps.DB, acquisitionID)
	if err != nil {
		return err
	}
	if a.ClientKind != "qbittorrent" {
		return errors.New("unsupported_client_kind")
	}
	if a.ExternalID == nil || strings.TrimSpace(*a.ExternalID) == "" {
		_ = storage.MarkAcquisitionFailed(ctx, deps.DB, a.ID, "missing_hash")
		return errors.New("missing_hash")
	}
	cfg, err := loadQBittorrentConfig(ctx, AcquisitionDeps{DB: deps.DB, SecretBox: deps.SecretBox, Policy: deps.Policy})
	if err != nil {
		return err
	}
	cli, err := qbittorrent.New(cfg, deps.Policy)
	if err != nil {
		return err
	}
	info, ok, err := cli.GetInfo(ctx, *a.ExternalID)
	if err != nil {
		if deps.Hub != nil {
			deps.Hub.Publish("download.failed", map[string]any{
				"acquisitionId": a.ID,
				"candidateId":   a.CandidateID,
				"targetId":      a.TargetID,
				"clientKind":    a.ClientKind,
				"clientName":    a.ClientName,
				"error":         err.Error(),
			})
		}
		return err
	}
	if !ok {
		// Not visible yet: retry later.
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_monitor", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(deps.PollEvery)})
		return nil
	}

	// Ensure we have download start/progress-change timestamps.
	now := time.Now().UTC()
	if a.Status == "pending" || a.Status == "enqueued" {
		_ = storage.MarkAcquisitionDownloading(ctx, deps.DB, a.ID, now, now)
	}

	progress := info.Progress
	oldProgress := 0.0
	if a.Progress != nil {
		oldProgress = *a.Progress
	}
	_ = storage.UpdateAcquisitionProgress(ctx, deps.DB, a.ID, "downloading", &progress, strPtr(info.ContentPath))
	if progress > oldProgress+0.001 {
		_ = storage.TouchAcquisitionProgressChanged(ctx, deps.DB, a.ID, now)
	}
	if deps.Hub != nil {
		deps.Hub.Publish("download.progress", map[string]any{
			"acquisitionId": a.ID,
			"candidateId":   a.CandidateID,
			"targetId":      a.TargetID,
			"clientKind":    a.ClientKind,
			"clientName":    a.ClientName,
			"hash":          a.ExternalID,
			"state":         info.State,
			"progress":      progress,
			"contentPath":   info.ContentPath,
			"savePath":      info.SavePath,
			"name":          info.Name,
		})
	}

	if qbittorrent.IsCompletedState(info.State) || progress >= 0.999 {
		// Mark completed.
		cp := strings.TrimSpace(info.ContentPath)
		if cp == "" {
			cp = filepath.Join(strings.TrimSpace(info.SavePath), strings.TrimSpace(info.Name))
		}
		_ = storage.MarkAcquisitionCompleted(ctx, deps.DB, a.ID, strPtr(cp))
		if deps.Hub != nil {
			deps.Hub.Publish("download.completed", map[string]any{
				"acquisitionId": a.ID,
				"candidateId":   a.CandidateID,
				"targetId":      a.TargetID,
				"clientKind":    a.ClientKind,
				"clientName":    a.ClientName,
				"contentPath":   cp,
			})
		}

		// Verify -> import pipeline.
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_verify", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(1 * time.Second)})
		return nil
	}

	// Stall detection: no progress change for too long.
	if a.ProgressChangedAt != nil && now.Sub(*a.ProgressChangedAt) > deps.StallAfter {
		_ = storage.MarkAcquisitionStalled(ctx, deps.DB, a.ID)
		if deps.Hub != nil {
			deps.Hub.Publish("download.stalled", map[string]any{
				"acquisitionId": a.ID,
				"candidateId":   a.CandidateID,
				"targetId":      a.TargetID,
				"progress":      progress,
				"state":         info.State,
			})
		}
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_recover", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(1 * time.Second)})
		return nil
	}

	// Not done: schedule monitor again.
	_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_monitor", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(deps.PollEvery)})
	return nil
}

func loadSetupPaths(ctx context.Context, db *sql.DB) (setupPaths, error) {
	v, ok, err := storage.GetSetting(ctx, db, "setup.paths")
	if err != nil {
		return setupPaths{}, err
	}
	if !ok {
		return setupPaths{}, errors.New("setup_paths_missing")
	}
	var p setupPaths
	if err := json.Unmarshal([]byte(v), &p); err != nil {
		return setupPaths{}, err
	}
	return p, nil
}

func strPtr(s string) *string {
	s = strings.TrimSpace(s)
	if s == "" {
		return nil
	}
	return &s
}
