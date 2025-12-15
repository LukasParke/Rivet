package queue

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"math"
	"math/rand"
	"time"

	"github.com/google/uuid"
)

func init() {
	rand.Seed(time.Now().UnixNano())
}

type Job struct {
	ID            string
	Kind          string
	State         string
	LeaseOwner    *string
	LeaseExpiresAt *time.Time
	Attempt       int
	RunAfter      time.Time
	PayloadJSON   *string
	LastError     *string
	CreatedAt     time.Time
	UpdatedAt     time.Time
}

type EnqueueOpts struct {
	RunAfter time.Time
}

func Enqueue(ctx context.Context, db *sql.DB, kind string, payload any, opts EnqueueOpts) (Job, error) {
	if kind == "" {
		return Job{}, errors.New("kind required")
	}
	now := time.Now().UTC()
	runAfter := opts.RunAfter
	if runAfter.IsZero() {
		runAfter = now
	}

	id := uuid.NewString()
	var payloadJSON *string
	if payload != nil {
		b, _ := json.Marshal(payload)
		s := string(b)
		payloadJSON = &s
	}

	_, err := db.ExecContext(ctx, `
INSERT INTO jobs (id, kind, state, lease_owner, lease_expires_at, attempt, run_after, payload_json, last_error, created_at, updated_at)
VALUES (?, ?, ?, NULL, NULL, 0, ?, ?, NULL, ?, ?)
`, id, kind, "queued", runAfter.Format(time.RFC3339Nano), payloadJSON, now.Format(time.RFC3339Nano), now.Format(time.RFC3339Nano))
	if err != nil {
		return Job{}, err
	}

	return Job{ID: id, Kind: kind, State: "queued", Attempt: 0, RunAfter: runAfter, PayloadJSON: payloadJSON, CreatedAt: now, UpdatedAt: now}, nil
}

type ClaimOpts struct {
	LeaseOwner    string
	LeaseDuration time.Duration
	Now           time.Time
}

var ErrNoJob = errors.New("no_job")

// ClaimOne claims a single available job (state=queued, run_after<=now, no active lease) and sets a lease.
func ClaimOne(ctx context.Context, db *sql.DB, opts ClaimOpts) (Job, error) {
	if opts.LeaseOwner == "" {
		opts.LeaseOwner = uuid.NewString()
	}
	if opts.LeaseDuration <= 0 {
		opts.LeaseDuration = 30 * time.Second
	}
	now := opts.Now
	if now.IsZero() {
		now = time.Now().UTC()
	}
	leaseExp := now.Add(opts.LeaseDuration)

	// Pick candidate ID.
	var id string
	err := db.QueryRowContext(ctx, `
SELECT id
FROM jobs
WHERE state = 'queued'
  AND run_after <= ?
  AND (lease_expires_at IS NULL OR lease_expires_at <= ?)
ORDER BY run_after ASC, created_at ASC
LIMIT 1
`, now.Format(time.RFC3339Nano), now.Format(time.RFC3339Nano)).Scan(&id)
	if err == sql.ErrNoRows {
		return Job{}, ErrNoJob
	}
	if err != nil {
		return Job{}, err
	}

	// Attempt to claim.
	res, err := db.ExecContext(ctx, `
UPDATE jobs
SET lease_owner = ?, lease_expires_at = ?, updated_at = ?
WHERE id = ?
  AND state = 'queued'
  AND run_after <= ?
  AND (lease_expires_at IS NULL OR lease_expires_at <= ?)
`, opts.LeaseOwner, leaseExp.Format(time.RFC3339Nano), now.Format(time.RFC3339Nano), id, now.Format(time.RFC3339Nano), now.Format(time.RFC3339Nano))
	if err != nil {
		return Job{}, err
	}
	affected, _ := res.RowsAffected()
	if affected != 1 {
		return Job{}, ErrNoJob
	}

	return Get(ctx, db, id)
}

func Get(ctx context.Context, db *sql.DB, id string) (Job, error) {
	var j Job
	var leaseOwner sql.NullString
	var leaseExp sql.NullString
	var payload sql.NullString
	var lastErr sql.NullString
	var runAfter, createdAt, updatedAt string

	err := db.QueryRowContext(ctx, `
SELECT id, kind, state, lease_owner, lease_expires_at, attempt, run_after, payload_json, last_error, created_at, updated_at
FROM jobs
WHERE id = ?
`, id).Scan(&j.ID, &j.Kind, &j.State, &leaseOwner, &leaseExp, &j.Attempt, &runAfter, &payload, &lastErr, &createdAt, &updatedAt)
	if err != nil {
		return Job{}, err
	}
	if leaseOwner.Valid {
		j.LeaseOwner = &leaseOwner.String
	}
	if leaseExp.Valid {
		t, _ := time.Parse(time.RFC3339Nano, leaseExp.String)
		j.LeaseExpiresAt = &t
	}
	if payload.Valid {
		j.PayloadJSON = &payload.String
	}
	if lastErr.Valid {
		j.LastError = &lastErr.String
	}
	j.RunAfter, _ = time.Parse(time.RFC3339Nano, runAfter)
	j.CreatedAt, _ = time.Parse(time.RFC3339Nano, createdAt)
	j.UpdatedAt, _ = time.Parse(time.RFC3339Nano, updatedAt)
	return j, nil
}

func Complete(ctx context.Context, db *sql.DB, id string, now time.Time) error {
	if now.IsZero() {
		now = time.Now().UTC()
	}
	_, err := db.ExecContext(ctx, `
UPDATE jobs
SET state = 'completed', lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
WHERE id = ?
`, now.Format(time.RFC3339Nano), id)
	return err
}

func FailAndRetry(ctx context.Context, db *sql.DB, id string, attempt int, errStr string, now time.Time) error {
	if now.IsZero() {
		now = time.Now().UTC()
	}
	next := nextBackoff(attempt, now)
	_, err := db.ExecContext(ctx, `
UPDATE jobs
SET state = 'queued', attempt = ?, run_after = ?, last_error = ?, lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
WHERE id = ?
`, attempt, next.Format(time.RFC3339Nano), errStr, now.Format(time.RFC3339Nano), id)
	return err
}

func FailFinal(ctx context.Context, db *sql.DB, id string, attempt int, errStr string, now time.Time) error {
	if now.IsZero() {
		now = time.Now().UTC()
	}
	_, err := db.ExecContext(ctx, `
UPDATE jobs
SET state = 'failed', attempt = ?, last_error = ?, lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
WHERE id = ?
`, attempt, errStr, now.Format(time.RFC3339Nano), id)
	return err
}

func nextBackoff(attempt int, now time.Time) time.Time {
	if attempt < 1 {
		attempt = 1
	}
	// Exponential backoff with jitter.
	base := time.Second * time.Duration(math.Min(60, math.Pow(2, float64(attempt))))
	jitter := time.Duration(rand.Int63n(int64(base/2 + 1)))
	return now.Add(base + jitter)
}
