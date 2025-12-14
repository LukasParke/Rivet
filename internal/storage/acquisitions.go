package storage

import (
	"context"
	"database/sql"
	"errors"
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
	ExternalID *string
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
	var createdAt, updatedAt string
	if err := db.QueryRowContext(ctx, `
SELECT id, candidate_id, target_id, kind, status, client_kind, client_name, external_id, last_error, created_at, updated_at
FROM acquisitions
WHERE id = ?
`, id).Scan(&a.ID, &a.CandidateID, &a.TargetID, &a.Kind, &a.Status, &a.ClientKind, &a.ClientName, &externalID, &lastErr, &createdAt, &updatedAt); err != nil {
		return Acquisition{}, err
	}
	if externalID.Valid {
		a.ExternalID = &externalID.String
	}
	if lastErr.Valid {
		a.LastError = &lastErr.String
	}
	a.CreatedAt, _ = time.Parse(time.RFC3339Nano, createdAt)
	a.UpdatedAt, _ = time.Parse(time.RFC3339Nano, updatedAt)
	return a, nil
}

func ListAcquisitions(ctx context.Context, db *sql.DB, limit int) ([]Acquisition, error) {
	if limit <= 0 || limit > 200 {
		limit = 50
	}
	rows, err := db.QueryContext(ctx, `
SELECT id, candidate_id, target_id, kind, status, client_kind, client_name, external_id, last_error, created_at, updated_at
FROM acquisitions
ORDER BY created_at DESC
LIMIT ?
`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var out []Acquisition
	for rows.Next() {
		var a Acquisition
		var externalID, lastErr sql.NullString
		var createdAt, updatedAt string
		if err := rows.Scan(&a.ID, &a.CandidateID, &a.TargetID, &a.Kind, &a.Status, &a.ClientKind, &a.ClientName, &externalID, &lastErr, &createdAt, &updatedAt); err != nil {
			return nil, err
		}
		if externalID.Valid {
			a.ExternalID = &externalID.String
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
