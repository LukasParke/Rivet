package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"time"

	importer "github.com/LukeHagar/Rivet/internal/import"
	"github.com/LukeHagar/Rivet/internal/jobs/queue"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/verify"
)

type VerifyDeps struct {
	DB  *sql.DB
	Hub publisher
}

func VerifyAcquisition(ctx context.Context, deps VerifyDeps, acquisitionID string) error {
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	a, err := storage.GetAcquisition(ctx, deps.DB, acquisitionID)
	if err != nil {
		return err
	}
	if a.ContentPath == nil || *a.ContentPath == "" {
		return errors.New("missing_content_path")
	}
	paths, err := loadSetupPaths(ctx, deps.DB)
	if err != nil {
		return err
	}
	if paths.QuarantinePath == "" {
		return errors.New("quarantine_path_missing")
	}

	_ = storage.MarkAcquisitionVerifying(ctx, deps.DB, a.ID)
	if deps.Hub != nil {
		deps.Hub.Publish("verify.started", map[string]any{"acquisitionId": a.ID, "contentPath": *a.ContentPath})
	}

	picked, err := importer.PickBestMediaFile(*a.ContentPath)
	if err != nil {
		_ = storage.MarkAcquisitionNeedsReview(ctx, deps.DB, a.ID, "failed", nil, err.Error())
		if deps.Hub != nil {
			deps.Hub.Publish("verify.failed", map[string]any{"acquisitionId": a.ID, "error": err.Error()})
		}
		// Rescout v1: try alternatives (missing/bad content path is common).
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_rescout", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(2 * time.Second)})
		return err
	}

	res, err := verify.VerifyFile(picked)
	if err != nil {
		_ = storage.MarkAcquisitionNeedsReview(ctx, deps.DB, a.ID, "error", nil, err.Error())
		if deps.Hub != nil {
			deps.Hub.Publish("verify.failed", map[string]any{"acquisitionId": a.ID, "error": err.Error()})
		}
		// Rescout v1: verifier errors may indicate partial/bad downloads.
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_rescout", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(2 * time.Second)})
		return err
	}
	if !res.OK {
		q, qErr := importer.Quarantine(picked, paths.QuarantinePath)
		var qPtr *string
		if qErr == nil {
			qPtr = &q
		}
		_ = storage.MarkAcquisitionNeedsReview(ctx, deps.DB, a.ID, res.Reason, qPtr, res.Reason)
		if deps.Hub != nil {
			deps.Hub.Publish("verify.failed", map[string]any{"acquisitionId": a.ID, "reason": res.Reason, "quarantinedPath": qPtr})
		}
		// Rescout v1: verification failure often means wrong release; try alternatives.
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_rescout", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(2 * time.Second)})
		return errors.New("verify_failed")
	}

	_ = storage.MarkAcquisitionVerified(ctx, deps.DB, a.ID, picked)
	if deps.Hub != nil {
		deps.Hub.Publish("verify.completed", map[string]any{"acquisitionId": a.ID, "verifiedPath": picked})
	}

	_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_import", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(1 * time.Second)})
	return nil
}

// keep compiler happy if security imported in other files
var _ = security.OutboundPolicy{}
var _ = json.RawMessage{}
