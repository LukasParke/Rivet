package handlers

import (
	"context"
	"database/sql"
	"errors"
	"strings"
	"time"

	"github.com/LukeHagar/Rivet/internal/adapters/downloaders/qbittorrent"
	"github.com/LukeHagar/Rivet/internal/jobs/queue"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type RecoverDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
	Hub       publisher
}

func RecoverAcquisition(ctx context.Context, deps RecoverDeps, acquisitionID string) error {
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
		return errors.New("missing_hash")
	}

	attempt, err := storage.IncrementAcquisitionRecoveryAttempt(ctx, deps.DB, a.ID)
	if err != nil {
		return err
	}

	if deps.Hub != nil {
		deps.Hub.Publish("download.recovery_attempt", map[string]any{"acquisitionId": a.ID, "attempt": attempt})
	}

	// Recovery ladder (MVP): resume -> reannounce -> recheck.
	cfg, err := loadQBittorrentConfig(ctx, AcquisitionDeps{DB: deps.DB, SecretBox: deps.SecretBox, Policy: deps.Policy})
	if err != nil {
		return err
	}
	cli, err := qbittorrent.New(cfg, deps.Policy)
	if err != nil {
		return err
	}

	var opErr error
	switch attempt {
	case 1:
		opErr = cli.Resume(ctx, *a.ExternalID)
	case 2:
		opErr = cli.Reannounce(ctx, *a.ExternalID)
	default:
		opErr = cli.Recheck(ctx, *a.ExternalID)
	}
	if opErr != nil {
		_ = storage.MarkAcquisitionFailed(ctx, deps.DB, a.ID, opErr.Error())
		if deps.Hub != nil {
			deps.Hub.Publish("download.recovery_failed", map[string]any{"acquisitionId": a.ID, "attempt": attempt, "error": opErr.Error()})
		}
		return opErr
	}

	// After a few attempts, escalate to review.
	if attempt >= 4 {
		_ = storage.MarkAcquisitionNeedsReview(ctx, deps.DB, a.ID, "stalled", a.QuarantinedPath, "stalled")
		if deps.Hub != nil {
			deps.Hub.Publish("download.needs_review", map[string]any{"acquisitionId": a.ID, "reason": "stalled"})
		}
		// Rescout v1: try to find an alternative and enqueue a replacement acquisition.
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_rescout", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(1 * time.Second)})
		return nil
	}

	// Resume monitoring quickly.
	_ = storage.TouchAcquisitionProgressChanged(ctx, deps.DB, a.ID, time.Now().UTC())
	_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_monitor", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(5 * time.Second)})
	if deps.Hub != nil {
		deps.Hub.Publish("download.recovery_scheduled", map[string]any{"acquisitionId": a.ID, "attempt": attempt})
	}
	return nil
}
