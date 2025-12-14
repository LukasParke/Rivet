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
	importer "github.com/LukeHagar/Rivet/internal/import"
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

	progress := info.Progress
	_ = storage.UpdateAcquisitionProgress(ctx, deps.DB, a.ID, "downloading", &progress, strPtr(info.ContentPath))
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

		// Import.
		if deps.Hub != nil {
			deps.Hub.Publish("import.started", map[string]any{
				"acquisitionId": a.ID,
				"candidateId":   a.CandidateID,
				"targetId":      a.TargetID,
				"contentPath":   cp,
			})
		}
		paths, err := loadSetupPaths(ctx, deps.DB)
		if err != nil {
			_ = storage.MarkAcquisitionFailed(ctx, deps.DB, a.ID, err.Error())
			if deps.Hub != nil {
				deps.Hub.Publish("import.failed", map[string]any{
					"acquisitionId": a.ID,
					"candidateId":   a.CandidateID,
					"targetId":      a.TargetID,
					"error":         err.Error(),
				})
			}
			return err
		}
		if len(paths.LibraryRoots) == 0 {
			_ = storage.MarkAcquisitionFailed(ctx, deps.DB, a.ID, "no_library_roots")
			if deps.Hub != nil {
				deps.Hub.Publish("import.failed", map[string]any{
					"acquisitionId": a.ID,
					"candidateId":   a.CandidateID,
					"targetId":      a.TargetID,
					"error":         "no_library_roots",
				})
			}
			return errors.New("no_library_roots")
		}
		// Minimal verification: choose a media file that is safe to import.
		src, err := importer.PickBestMediaFile(cp)
		if err != nil {
			_ = storage.MarkAcquisitionFailed(ctx, deps.DB, a.ID, err.Error())
			if deps.Hub != nil {
				deps.Hub.Publish("import.failed", map[string]any{
					"acquisitionId": a.ID,
					"candidateId":   a.CandidateID,
					"targetId":      a.TargetID,
					"error":         err.Error(),
				})
			}
			return err
		}
		dest, err := importer.ImportFile(src, paths.LibraryRoots[0])
		if err != nil {
			_ = storage.MarkAcquisitionFailed(ctx, deps.DB, a.ID, err.Error())
			if deps.Hub != nil {
				deps.Hub.Publish("import.failed", map[string]any{
					"acquisitionId": a.ID,
					"candidateId":   a.CandidateID,
					"targetId":      a.TargetID,
					"error":         err.Error(),
				})
			}
			return err
		}
		_ = storage.MarkAcquisitionImported(ctx, deps.DB, a.ID, dest)
		if deps.Hub != nil {
			deps.Hub.Publish("import.completed", map[string]any{
				"acquisitionId": a.ID,
				"candidateId":   a.CandidateID,
				"targetId":      a.TargetID,
				"importedPath":  dest,
			})
		}
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
