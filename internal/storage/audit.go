package storage

import (
	"context"
	"database/sql"
	"time"

	"github.com/google/uuid"
)

type AuditEvent struct {
	ID          string
	ActorUserID *string
	Action      string
	TargetType  *string
	TargetID    *string
	IP          *string
	UserAgent   *string
	DetailsJSON *string
	CreatedAt   time.Time
}

func InsertAuditEvent(ctx context.Context, db *sql.DB, e AuditEvent) error {
	if e.ID == "" {
		e.ID = uuid.NewString()
	}
	if e.CreatedAt.IsZero() {
		e.CreatedAt = time.Now().UTC()
	}

	_, err := db.ExecContext(ctx, `
INSERT INTO audit_events (id, actor_user_id, action, target_type, target_id, ip, user_agent, details_json, created_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
`, e.ID, e.ActorUserID, e.Action, e.TargetType, e.TargetID, e.IP, e.UserAgent, e.DetailsJSON, e.CreatedAt.Format(time.RFC3339Nano))
	return err
}
