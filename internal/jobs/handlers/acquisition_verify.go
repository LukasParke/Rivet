package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"path/filepath"
	"strings"
	"time"

	importer "github.com/LukeHagar/Rivet/internal/import"
	"github.com/LukeHagar/Rivet/internal/jobs/queue"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/verify"
)

func mustJSON(v any) string {
	b, _ := json.Marshal(v)
	return string(b)
}

type VerifyDeps struct {
	DB  *sql.DB
	Hub publisher
	DryRun            bool
	DryRunFixturePath string
	FFProbePath       string
}

func VerifyAcquisition(ctx context.Context, deps VerifyDeps, acquisitionID string) error {
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	a, err := storage.GetAcquisition(ctx, deps.DB, acquisitionID)
	if err != nil {
		return err
	}
	contentPath := ""
	if a.ContentPath != nil {
		contentPath = strings.TrimSpace(*a.ContentPath)
	}
	if deps.DryRun {
		// Prefer a local fixture for read-only verification/import.
		if fp := strings.TrimSpace(deps.DryRunFixturePath); fp != "" {
			contentPath = fp
		}
		// If no fixture is set, allow continuing with a synthetic path (no file IO).
		if contentPath == "" {
			contentPath = "dryrun://content"
		}
	} else {
		if contentPath == "" {
			return errors.New("missing_content_path")
		}
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
		deps.Hub.Publish("verify.started", map[string]any{"acquisitionId": a.ID, "contentPath": contentPath, "dryRun": deps.DryRun})
	}

	// If we have no real file/dir to inspect (common in dry run), short-circuit.
	if deps.DryRun && strings.HasPrefix(contentPath, "dryrun://") {
		_ = storage.SetAcquisitionVerifyEvidence(ctx, deps.DB, a.ID, mustJSON(map[string]any{
			"dryRun": true,
			"picked": contentPath,
			"result": map[string]any{"ok": true, "reason": "dry_run_no_fixture"},
		}))
		_ = storage.MarkAcquisitionVerified(ctx, deps.DB, a.ID, contentPath)
		if deps.Hub != nil {
			deps.Hub.Publish("verify.completed", map[string]any{"acquisitionId": a.ID, "verifiedPath": contentPath, "dryRun": true})
		}
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_import", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(1 * time.Second)})
		return nil
	}

	picked, err := importer.PickBestMediaFile(contentPath)
	if err != nil {
		_ = storage.SetAcquisitionVerifyEvidence(ctx, deps.DB, a.ID, mustJSON(map[string]any{
			"dryRun": deps.DryRun,
			"contentPath": contentPath,
			"error": err.Error(),
		}))
		_ = storage.MarkAcquisitionNeedsReview(ctx, deps.DB, a.ID, "failed", nil, err.Error())
		if deps.Hub != nil {
			deps.Hub.Publish("verify.failed", map[string]any{"acquisitionId": a.ID, "error": err.Error(), "dryRun": deps.DryRun})
		}
		// Rescout v1: try alternatives (missing/bad content path is common).
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_rescout", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(2 * time.Second)})
		return err
	}

	res, err := verify.VerifyFile(picked)
	if err != nil {
		_ = storage.SetAcquisitionVerifyEvidence(ctx, deps.DB, a.ID, mustJSON(map[string]any{
			"dryRun": deps.DryRun,
			"picked": picked,
			"error": err.Error(),
		}))
		_ = storage.MarkAcquisitionNeedsReview(ctx, deps.DB, a.ID, "error", nil, err.Error())
		if deps.Hub != nil {
			deps.Hub.Publish("verify.failed", map[string]any{"acquisitionId": a.ID, "error": err.Error(), "dryRun": deps.DryRun})
		}
		// Rescout v1: verifier errors may indicate partial/bad downloads.
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_rescout", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(2 * time.Second)})
		return err
	}
	// Optional deeper inspection via ffprobe (if available).
	var probe any
	if !deps.DryRun {
		requireFFProbe := strings.TrimSpace(deps.FFProbePath) != ""
		if sum, ok, pErr := verify.ProbeFFProbe(ctx, deps.FFProbePath, picked); ok {
			if pErr == nil {
				probe = sum
				// Only enforce ffprobe-derived rules when explicitly configured.
				if requireFFProbe && sum.DurationSeconds > 0 && sum.DurationSeconds < 5*60 {
					res.OK = false
					res.Reason = "duration_too_short"
				}
			} else {
				probe = map[string]any{"error": pErr.Error()}
				if requireFFProbe {
					res.OK = false
					res.Reason = "ffprobe_failed"
				}
			}
		}
	}

	_ = storage.SetAcquisitionVerifyEvidence(ctx, deps.DB, a.ID, mustJSON(map[string]any{
		"dryRun": deps.DryRun,
		"picked": picked,
		"result": res,
		"ffprobe": probe,
	}))

	if !res.OK {
		var qPtr *string
		if deps.DryRun {
			planned := "dryrun://quarantine/" + filepath.Base(picked)
			qPtr = &planned
		} else {
			q, qErr := importer.Quarantine(picked, paths.QuarantinePath)
			if qErr == nil {
				qPtr = &q
			}
		}
		_ = storage.MarkAcquisitionNeedsReview(ctx, deps.DB, a.ID, res.Reason, qPtr, res.Reason)
		if deps.Hub != nil {
			deps.Hub.Publish("verify.failed", map[string]any{"acquisitionId": a.ID, "reason": res.Reason, "quarantinedPath": qPtr, "dryRun": deps.DryRun})
		}
		// Rescout v1: verification failure often means wrong release; try alternatives.
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_rescout", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(2 * time.Second)})
		return errors.New("verify_failed")
	}

	_ = storage.MarkAcquisitionVerified(ctx, deps.DB, a.ID, picked)
	if deps.Hub != nil {
		deps.Hub.Publish("verify.completed", map[string]any{"acquisitionId": a.ID, "verifiedPath": picked, "dryRun": deps.DryRun})
	}

	_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_import", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(1 * time.Second)})
	return nil
}

// keep compiler happy if security imported in other files
var _ = security.OutboundPolicy{}
var _ = json.RawMessage{}
