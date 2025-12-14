package storage

import (
	"context"
	"database/sql"
	"encoding/json"
	"time"

	"github.com/google/uuid"
)

type WantedTarget struct {
	ID             string
	Kind           string
	ExternalSource string
	ExternalID     string
	State          string
	PayloadJSON    *string
	CreatedAt      time.Time
	UpdatedAt      time.Time
}

type SearchRun struct {
	ID          string
	TargetID    string
	Status      string
	StartedAt   *time.Time
	CompletedAt *time.Time
	Error       *string
}

type ReleaseCandidate struct {
	ID            string
	SearchRunID   string
	IndexerName   string
	RawJSON       string
	NormalizedJSON *string
	CreatedAt     time.Time
}

type Decision struct {
	ID          string
	CandidateID string
	Accepted    bool
	Score       int
	ReasonsJSON string
	CreatedAt   time.Time
}

func CreateWantedTarget(ctx context.Context, db *sql.DB, kind, externalSource, externalID string, payload any) (WantedTarget, error) {
	now := time.Now().UTC()
	id := uuid.NewString()
	var payloadJSON *string
	if payload != nil {
		b, _ := json.Marshal(payload)
		s := string(b)
		payloadJSON = &s
	}

	_, err := db.ExecContext(ctx, `
INSERT INTO wanted_targets (id, kind, external_source, external_id, state, payload_json, created_at, updated_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?)
`, id, kind, externalSource, externalID, "wanted", payloadJSON, now.Format(time.RFC3339Nano), now.Format(time.RFC3339Nano))
	if err != nil {
		return WantedTarget{}, err
	}
	return WantedTarget{ID: id, Kind: kind, ExternalSource: externalSource, ExternalID: externalID, State: "wanted", PayloadJSON: payloadJSON, CreatedAt: now, UpdatedAt: now}, nil
}

func ListWantedTargets(ctx context.Context, db *sql.DB, limit int) ([]WantedTarget, error) {
	if limit <= 0 || limit > 200 {
		limit = 50
	}
	rows, err := db.QueryContext(ctx, `
SELECT id, kind, external_source, external_id, state, payload_json, created_at, updated_at
FROM wanted_targets
ORDER BY created_at DESC
LIMIT ?
`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var out []WantedTarget
	for rows.Next() {
		var t WantedTarget
		var payload sql.NullString
		var createdAt, updatedAt string
		if err := rows.Scan(&t.ID, &t.Kind, &t.ExternalSource, &t.ExternalID, &t.State, &payload, &createdAt, &updatedAt); err != nil {
			return nil, err
		}
		if payload.Valid {
			t.PayloadJSON = &payload.String
		}
		ct, err := time.Parse(time.RFC3339Nano, createdAt)
		if err != nil {
			return nil, err
		}
		ut, err := time.Parse(time.RFC3339Nano, updatedAt)
		if err != nil {
			return nil, err
		}
		t.CreatedAt = ct
		t.UpdatedAt = ut
		out = append(out, t)
	}
	return out, rows.Err()
}

func CreateSearchRun(ctx context.Context, db *sql.DB, targetID string) (SearchRun, error) {
	id := uuid.NewString()
	_, err := db.ExecContext(ctx, `INSERT INTO search_runs (id, target_id, status) VALUES (?, ?, ?)`, id, targetID, "queued")
	if err != nil {
		return SearchRun{}, err
	}
	return SearchRun{ID: id, TargetID: targetID, Status: "queued"}, nil
}

func UpdateSearchRunRunning(ctx context.Context, db *sql.DB, runID string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `UPDATE search_runs SET status = ?, started_at = ? WHERE id = ?`, "running", now.Format(time.RFC3339Nano), runID)
	return err
}

func UpdateSearchRunCompleted(ctx context.Context, db *sql.DB, runID string, errStr *string) error {
	now := time.Now().UTC()
	status := "completed"
	if errStr != nil {
		status = "failed"
	}
	_, err := db.ExecContext(ctx, `UPDATE search_runs SET status = ?, completed_at = ?, error = ? WHERE id = ?`, status, now.Format(time.RFC3339Nano), errStr, runID)
	return err
}

func InsertReleaseCandidate(ctx context.Context, db *sql.DB, runID, indexerName, rawJSON string, normalizedJSON *string) (ReleaseCandidate, error) {
	now := time.Now().UTC()
	id := uuid.NewString()
	_, err := db.ExecContext(ctx, `
INSERT INTO release_candidates (id, search_run_id, indexer_name, raw_json, normalized_json, created_at)
VALUES (?, ?, ?, ?, ?, ?)
`, id, runID, indexerName, rawJSON, normalizedJSON, now.Format(time.RFC3339Nano))
	if err != nil {
		return ReleaseCandidate{}, err
	}
	return ReleaseCandidate{ID: id, SearchRunID: runID, IndexerName: indexerName, RawJSON: rawJSON, NormalizedJSON: normalizedJSON, CreatedAt: now}, nil
}

func InsertDecision(ctx context.Context, db *sql.DB, candidateID string, accepted bool, score int, reasons any) (Decision, error) {
	now := time.Now().UTC()
	id := uuid.NewString()
	b, _ := json.Marshal(reasons)
	reasonsJSON := string(b)
	acc := 0
	if accepted {
		acc = 1
	}
	_, err := db.ExecContext(ctx, `
INSERT INTO decisions (id, candidate_id, accepted, score, reasons_json, created_at)
VALUES (?, ?, ?, ?, ?, ?)
`, id, candidateID, acc, score, reasonsJSON, now.Format(time.RFC3339Nano))
	if err != nil {
		return Decision{}, err
	}
	return Decision{ID: id, CandidateID: candidateID, Accepted: accepted, Score: score, ReasonsJSON: reasonsJSON, CreatedAt: now}, nil
}

type CandidateWithDecision struct {
	Candidate ReleaseCandidate
	Decision  *Decision
}

func GetSearchRunWithResults(ctx context.Context, db *sql.DB, runID string) (SearchRun, []CandidateWithDecision, error) {
	var run SearchRun
	var startedAt, completedAt, errStr sql.NullString
	row := db.QueryRowContext(ctx, `SELECT id, target_id, status, started_at, completed_at, error FROM search_runs WHERE id = ?`, runID)
	if err := row.Scan(&run.ID, &run.TargetID, &run.Status, &startedAt, &completedAt, &errStr); err != nil {
		return SearchRun{}, nil, err
	}
	if startedAt.Valid {
		t, err := time.Parse(time.RFC3339Nano, startedAt.String)
		if err == nil {
			run.StartedAt = &t
		}
	}
	if completedAt.Valid {
		t, err := time.Parse(time.RFC3339Nano, completedAt.String)
		if err == nil {
			run.CompletedAt = &t
		}
	}
	if errStr.Valid {
		run.Error = &errStr.String
	}

	rows, err := db.QueryContext(ctx, `
SELECT c.id, c.search_run_id, c.indexer_name, c.raw_json, c.normalized_json, c.created_at,
       d.id, d.accepted, d.score, d.reasons_json, d.created_at
FROM release_candidates c
LEFT JOIN decisions d ON d.candidate_id = c.id
WHERE c.search_run_id = ?
ORDER BY c.created_at ASC
`, runID)
	if err != nil {
		return run, nil, err
	}
	defer rows.Close()

	var out []CandidateWithDecision
	for rows.Next() {
		var c ReleaseCandidate
		var norm sql.NullString
		var cCreated string
		var dID sql.NullString
		var dAccepted sql.NullInt64
		var dScore sql.NullInt64
		var dReasons sql.NullString
		var dCreated sql.NullString

		if err := rows.Scan(&c.ID, &c.SearchRunID, &c.IndexerName, &c.RawJSON, &norm, &cCreated, &dID, &dAccepted, &dScore, &dReasons, &dCreated); err != nil {
			return run, nil, err
		}
		if norm.Valid {
			c.NormalizedJSON = &norm.String
		}
		t, err := time.Parse(time.RFC3339Nano, cCreated)
		if err == nil {
			c.CreatedAt = t
		}

		var d *Decision
		if dID.Valid {
			dt := Decision{ID: dID.String, CandidateID: c.ID, Accepted: dAccepted.Int64 == 1, Score: int(dScore.Int64)}
			if dReasons.Valid {
				dt.ReasonsJSON = dReasons.String
			}
			if dCreated.Valid {
				if tt, err := time.Parse(time.RFC3339Nano, dCreated.String); err == nil {
					dt.CreatedAt = tt
				}
			}
			d = &dt
		}

		out = append(out, CandidateWithDecision{Candidate: c, Decision: d})
	}
	return run, out, rows.Err()
}
