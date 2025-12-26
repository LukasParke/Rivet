package storage

import (
	"context"
	"database/sql"
	"errors"
	"strings"
	"time"

	"github.com/google/uuid"
)

type Acquisition struct {
	ID         string
	CandidateID string
	TargetID   string
	Kind       string
	Status     string
	ClientKind string
	ClientName string
	SourceURL  *string
	ExternalID *string
	ContentPath *string
	VerifiedPath *string
	QuarantinedPath *string
	VerifyStatus *string
	VerifyEvidenceJSON *string
	ImportedPath *string
	CompletedAt  *time.Time
	ImportedAt   *time.Time
	Progress     *float64
	DownloadStartedAt *time.Time
	ProgressChangedAt *time.Time
	StalledAt         *time.Time
	RecoveryAttempt   int
	LastError  *string
	CreatedAt  time.Time
	UpdatedAt  time.Time
}

func CreateAcquisition(ctx context.Context, db *sql.DB, candidateID, targetID, kind, clientKind, clientName string) (Acquisition, error) {
	if candidateID == "" || targetID == "" {
		return Acquisition{}, errors.New("candidateID and targetID required")
	}
	now := time.Now().UTC()
	id := uuid.NewString()
	_, err := db.ExecContext(ctx, `
INSERT INTO acquisitions (id, candidate_id, target_id, kind, status, client_kind, client_name, external_id, last_error, created_at, updated_at)
VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)
`, id, candidateID, targetID, kind, "pending", clientKind, clientName, now.Format(time.RFC3339Nano), now.Format(time.RFC3339Nano))
	if err != nil {
		return Acquisition{}, err
	}
	return Acquisition{ID: id, CandidateID: candidateID, TargetID: targetID, Kind: kind, Status: "pending", ClientKind: clientKind, ClientName: clientName, CreatedAt: now, UpdatedAt: now}, nil
}

func MarkAcquisitionEnqueued(ctx context.Context, db *sql.DB, id string, externalID *string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET status = 'enqueued', external_id = ?, last_error = NULL, updated_at = ?
WHERE id = ?
`, externalID, now.Format(time.RFC3339Nano), id)
	return err
}

func UpdateAcquisitionProgress(ctx context.Context, db *sql.DB, id string, status string, progress *float64, contentPath *string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET status = ?, progress = ?, content_path = COALESCE(?, content_path), updated_at = ?
WHERE id = ?
`, status, progress, contentPath, now.Format(time.RFC3339Nano), id)
	return err
}

func MarkAcquisitionDownloading(ctx context.Context, db *sql.DB, id string, downloadStartedAt time.Time, progressChangedAt time.Time) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET status = 'downloading',
    download_started_at = COALESCE(download_started_at, ?),
    progress_changed_at = COALESCE(progress_changed_at, ?),
    stalled_at = NULL,
    updated_at = ?
WHERE id = ?
`, downloadStartedAt.Format(time.RFC3339Nano), progressChangedAt.Format(time.RFC3339Nano), now.Format(time.RFC3339Nano), id)
	return err
}

func TouchAcquisitionProgressChanged(ctx context.Context, db *sql.DB, id string, when time.Time) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET progress_changed_at = ?, updated_at = ?
WHERE id = ?
`, when.Format(time.RFC3339Nano), now.Format(time.RFC3339Nano), id)
	return err
}

func MarkAcquisitionStalled(ctx context.Context, db *sql.DB, id string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET status = 'stalled', stalled_at = ?, updated_at = ?
WHERE id = ?
`, now.Format(time.RFC3339Nano), now.Format(time.RFC3339Nano), id)
	return err
}

func IncrementAcquisitionRecoveryAttempt(ctx context.Context, db *sql.DB, id string) (int, error) {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET recovery_attempt = COALESCE(recovery_attempt, 0) + 1, updated_at = ?
WHERE id = ?
`, now.Format(time.RFC3339Nano), id)
	if err != nil {
		return 0, err
	}
	var n int
	_ = db.QueryRowContext(ctx, `SELECT COALESCE(recovery_attempt, 0) FROM acquisitions WHERE id = ?`, id).Scan(&n)
	return n, nil
}

func MarkAcquisitionVerifying(ctx context.Context, db *sql.DB, id string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET status = 'verifying', verify_status = 'running', last_error = NULL, updated_at = ?
WHERE id = ?
`, now.Format(time.RFC3339Nano), id)
	return err
}

func MarkAcquisitionVerified(ctx context.Context, db *sql.DB, id string, verifiedPath string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET verify_status = 'passed', verified_path = ?, quarantined_path = NULL, updated_at = ?
WHERE id = ?
`, verifiedPath, now.Format(time.RFC3339Nano), id)
	return err
}

func MarkAcquisitionNeedsReview(ctx context.Context, db *sql.DB, id string, verifyStatus string, quarantinedPath *string, errStr string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET status = 'needs_review', verify_status = ?, quarantined_path = ?, last_error = ?, updated_at = ?
WHERE id = ?
`, verifyStatus, quarantinedPath, errStr, now.Format(time.RFC3339Nano), id)
	return err
}

func SetAcquisitionVerifyEvidence(ctx context.Context, db *sql.DB, id string, evidenceJSON string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET verify_evidence_json = ?, updated_at = ?
WHERE id = ?
`, evidenceJSON, now.Format(time.RFC3339Nano), id)
	return err
}

// AdminAcceptAcquisitionVerified marks an acquisition as verified (passed) and clears it from the review queue.
// It does not touch the filesystem; the caller should enqueue import if desired.
func AdminAcceptAcquisitionVerified(ctx context.Context, db *sql.DB, id string, verifiedPath string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET status = 'completed',
    verify_status = 'passed',
    verified_path = ?,
    quarantined_path = NULL,
    last_error = NULL,
    updated_at = ?
WHERE id = ?
`, verifiedPath, now.Format(time.RFC3339Nano), id)
	return err
}

func MarkAcquisitionCompleted(ctx context.Context, db *sql.DB, id string, contentPath *string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET status = 'completed', completed_at = ?, content_path = ?, updated_at = ?
WHERE id = ?
`, now.Format(time.RFC3339Nano), contentPath, now.Format(time.RFC3339Nano), id)
	return err
}

func MarkAcquisitionImported(ctx context.Context, db *sql.DB, id string, importedPath string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET status = 'imported', imported_at = ?, imported_path = ?, updated_at = ?
WHERE id = ?
`, now.Format(time.RFC3339Nano), importedPath, now.Format(time.RFC3339Nano), id)
	return err
}

func MarkAcquisitionFailed(ctx context.Context, db *sql.DB, id string, errStr string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `
UPDATE acquisitions
SET status = 'failed', last_error = ?, updated_at = ?
WHERE id = ?
`, errStr, now.Format(time.RFC3339Nano), id)
	return err
}

func GetAcquisitionByCandidate(ctx context.Context, db *sql.DB, candidateID string) (Acquisition, bool, error) {
	var a Acquisition
	var externalID, lastErr sql.NullString
	var createdAt, updatedAt string
	err := db.QueryRowContext(ctx, `
SELECT id, candidate_id, target_id, kind, status, client_kind, client_name, external_id, last_error, created_at, updated_at
FROM acquisitions
WHERE candidate_id = ?
`, candidateID).Scan(&a.ID, &a.CandidateID, &a.TargetID, &a.Kind, &a.Status, &a.ClientKind, &a.ClientName, &externalID, &lastErr, &createdAt, &updatedAt)
	if err == sql.ErrNoRows {
		return Acquisition{}, false, nil
	}
	if err != nil {
		return Acquisition{}, false, err
	}
	if externalID.Valid {
		a.ExternalID = &externalID.String
	}
	if lastErr.Valid {
		a.LastError = &lastErr.String
	}
	a.CreatedAt, _ = time.Parse(time.RFC3339Nano, createdAt)
	a.UpdatedAt, _ = time.Parse(time.RFC3339Nano, updatedAt)
	return a, true, nil
}

func GetAcquisition(ctx context.Context, db *sql.DB, id string) (Acquisition, error) {
	var a Acquisition
	var externalID, lastErr sql.NullString
	var sourceURL, contentPath, importedPath sql.NullString
	var verifiedPath, quarantinedPath, verifyStatus sql.NullString
	var verifyEvidence sql.NullString
	var completedAt, importedAt sql.NullString
	var progress sql.NullFloat64
	var downloadStartedAt, progressChangedAt, stalledAt sql.NullString
	var recoveryAttempt sql.NullInt64
	var createdAt, updatedAt string
	if err := db.QueryRowContext(ctx, `
SELECT id, candidate_id, target_id, kind, status, client_kind, client_name, source_url, external_id, content_path, verified_path, quarantined_path, verify_status, verify_evidence_json, imported_path, completed_at, imported_at, progress, download_started_at, progress_changed_at, stalled_at, COALESCE(recovery_attempt, 0), last_error, created_at, updated_at
FROM acquisitions
WHERE id = ?
`, id).Scan(&a.ID, &a.CandidateID, &a.TargetID, &a.Kind, &a.Status, &a.ClientKind, &a.ClientName, &sourceURL, &externalID, &contentPath, &verifiedPath, &quarantinedPath, &verifyStatus, &verifyEvidence, &importedPath, &completedAt, &importedAt, &progress, &downloadStartedAt, &progressChangedAt, &stalledAt, &recoveryAttempt, &lastErr, &createdAt, &updatedAt); err != nil {
		return Acquisition{}, err
	}
	if sourceURL.Valid {
		a.SourceURL = &sourceURL.String
	}
	if externalID.Valid {
		a.ExternalID = &externalID.String
	}
	if contentPath.Valid {
		a.ContentPath = &contentPath.String
	}
	if verifiedPath.Valid {
		a.VerifiedPath = &verifiedPath.String
	}
	if quarantinedPath.Valid {
		a.QuarantinedPath = &quarantinedPath.String
	}
	if verifyStatus.Valid {
		a.VerifyStatus = &verifyStatus.String
	}
	if verifyEvidence.Valid {
		a.VerifyEvidenceJSON = &verifyEvidence.String
	}
	if importedPath.Valid {
		a.ImportedPath = &importedPath.String
	}
	if completedAt.Valid {
		if t, err := time.Parse(time.RFC3339Nano, completedAt.String); err == nil {
			a.CompletedAt = &t
		}
	}
	if importedAt.Valid {
		if t, err := time.Parse(time.RFC3339Nano, importedAt.String); err == nil {
			a.ImportedAt = &t
		}
	}
	if progress.Valid {
		v := progress.Float64
		a.Progress = &v
	}
	if downloadStartedAt.Valid {
		if t, err := time.Parse(time.RFC3339Nano, downloadStartedAt.String); err == nil {
			a.DownloadStartedAt = &t
		}
	}
	if progressChangedAt.Valid {
		if t, err := time.Parse(time.RFC3339Nano, progressChangedAt.String); err == nil {
			a.ProgressChangedAt = &t
		}
	}
	if stalledAt.Valid {
		if t, err := time.Parse(time.RFC3339Nano, stalledAt.String); err == nil {
			a.StalledAt = &t
		}
	}
	if recoveryAttempt.Valid {
		a.RecoveryAttempt = int(recoveryAttempt.Int64)
	}
	if lastErr.Valid {
		a.LastError = &lastErr.String
	}
	a.CreatedAt, _ = time.Parse(time.RFC3339Nano, createdAt)
	a.UpdatedAt, _ = time.Parse(time.RFC3339Nano, updatedAt)
	return a, nil
}

func ListAcquisitions(ctx context.Context, db *sql.DB, limit int) ([]Acquisition, error) {
	return listAcquisitions(ctx, db, limit, nil)
}

func ListAcquisitionsByStatus(ctx context.Context, db *sql.DB, status string, limit int) ([]Acquisition, error) {
	status = strings.TrimSpace(status)
	if status == "" {
		return listAcquisitions(ctx, db, limit, nil)
	}
	return listAcquisitions(ctx, db, limit, &status)
}

func listAcquisitions(ctx context.Context, db *sql.DB, limit int, status *string) ([]Acquisition, error) {
	if limit <= 0 || limit > 200 {
		limit = 50
	}
	q := `
SELECT id, candidate_id, target_id, kind, status, client_kind, client_name,
       source_url, external_id, content_path, verified_path, quarantined_path, verify_status, verify_evidence_json, imported_path,
       completed_at, imported_at, progress,
       download_started_at, progress_changed_at, stalled_at, COALESCE(recovery_attempt, 0),
       last_error, created_at, updated_at
FROM acquisitions
`
	args := []any{}
	if status != nil {
		q += "WHERE status = ?\n"
		args = append(args, *status)
	}
	q += `
ORDER BY created_at DESC
LIMIT ?
`
	args = append(args, limit)
	rows, err := db.QueryContext(ctx, q, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var out []Acquisition
	for rows.Next() {
		var a Acquisition
		var sourceURL, externalID, contentPath, importedPath sql.NullString
		var verifiedPath, quarantinedPath, verifyStatus sql.NullString
		var verifyEvidence sql.NullString
		var completedAt, importedAt sql.NullString
		var progress sql.NullFloat64
		var downloadStartedAt, progressChangedAt, stalledAt sql.NullString
		var recoveryAttempt sql.NullInt64
		var lastErr sql.NullString
		var createdAt, updatedAt string
		if err := rows.Scan(&a.ID, &a.CandidateID, &a.TargetID, &a.Kind, &a.Status, &a.ClientKind, &a.ClientName,
			&sourceURL, &externalID, &contentPath, &verifiedPath, &quarantinedPath, &verifyStatus, &verifyEvidence, &importedPath,
			&completedAt, &importedAt, &progress,
			&downloadStartedAt, &progressChangedAt, &stalledAt, &recoveryAttempt,
			&lastErr, &createdAt, &updatedAt); err != nil {
			return nil, err
		}
		if sourceURL.Valid {
			a.SourceURL = &sourceURL.String
		}
		if externalID.Valid {
			a.ExternalID = &externalID.String
		}
		if contentPath.Valid {
			a.ContentPath = &contentPath.String
		}
		if verifiedPath.Valid {
			a.VerifiedPath = &verifiedPath.String
		}
		if quarantinedPath.Valid {
			a.QuarantinedPath = &quarantinedPath.String
		}
		if verifyStatus.Valid {
			a.VerifyStatus = &verifyStatus.String
		}
		if verifyEvidence.Valid {
			a.VerifyEvidenceJSON = &verifyEvidence.String
		}
		if importedPath.Valid {
			a.ImportedPath = &importedPath.String
		}
		if completedAt.Valid {
			if t, err := time.Parse(time.RFC3339Nano, completedAt.String); err == nil {
				a.CompletedAt = &t
			}
		}
		if importedAt.Valid {
			if t, err := time.Parse(time.RFC3339Nano, importedAt.String); err == nil {
				a.ImportedAt = &t
			}
		}
		if progress.Valid {
			v := progress.Float64
			a.Progress = &v
		}
		if downloadStartedAt.Valid {
			if t, err := time.Parse(time.RFC3339Nano, downloadStartedAt.String); err == nil {
				a.DownloadStartedAt = &t
			}
		}
		if progressChangedAt.Valid {
			if t, err := time.Parse(time.RFC3339Nano, progressChangedAt.String); err == nil {
				a.ProgressChangedAt = &t
			}
		}
		if stalledAt.Valid {
			if t, err := time.Parse(time.RFC3339Nano, stalledAt.String); err == nil {
				a.StalledAt = &t
			}
		}
		if recoveryAttempt.Valid {
			a.RecoveryAttempt = int(recoveryAttempt.Int64)
		}
		if lastErr.Valid {
			a.LastError = &lastErr.String
		}
		a.CreatedAt, _ = time.Parse(time.RFC3339Nano, createdAt)
		a.UpdatedAt, _ = time.Parse(time.RFC3339Nano, updatedAt)
		out = append(out, a)
	}
	return out, rows.Err()
}
