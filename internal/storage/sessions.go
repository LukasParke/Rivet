package storage

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/base64"
	"errors"
	"time"

	"github.com/google/uuid"
)

type Session struct {
	ID        string
	UserID    string
	CSRFToken string

	CreatedAt  time.Time
	ExpiresAt  time.Time
	RevokedAt  *time.Time
	LastSeenAt *time.Time

	IP        *string
	UserAgent *string
}

func newToken(n int) (string, error) {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return base64.RawStdEncoding.EncodeToString(b), nil
}

func CreateSession(ctx context.Context, db *sql.DB, userID string, ttl time.Duration, ip, userAgent *string) (Session, error) {
	if userID == "" {
		return Session{}, errors.New("userID required")
	}
	if ttl <= 0 {
		return Session{}, errors.New("ttl must be > 0")
	}

	now := time.Now().UTC()
	csrf, err := newToken(32)
	if err != nil {
		return Session{}, err
	}

	s := Session{
		ID:        uuid.NewString(),
		UserID:    userID,
		CSRFToken: csrf,
		CreatedAt: now,
		ExpiresAt: now.Add(ttl),
		IP:        ip,
		UserAgent: userAgent,
	}

	_, err = db.ExecContext(ctx, `
INSERT INTO sessions (id, user_id, csrf_token, created_at, expires_at, revoked_at, last_seen_at, ip, user_agent)
VALUES (?, ?, ?, ?, ?, NULL, NULL, ?, ?)
`, s.ID, s.UserID, s.CSRFToken, s.CreatedAt.Format(time.RFC3339Nano), s.ExpiresAt.Format(time.RFC3339Nano), s.IP, s.UserAgent)
	if err != nil {
		return Session{}, err
	}
	return s, nil
}

func GetSession(ctx context.Context, db *sql.DB, sessionID string) (Session, error) {
	var s Session
	var revokedAt, lastSeenAt sql.NullString
	var ip, ua sql.NullString

	row := db.QueryRowContext(ctx, `
SELECT id, user_id, COALESCE(csrf_token, ''), created_at, expires_at, revoked_at, last_seen_at, ip, user_agent
FROM sessions
WHERE id = ?
`, sessionID)

	var createdAt, expiresAt string
	if err := row.Scan(&s.ID, &s.UserID, &s.CSRFToken, &createdAt, &expiresAt, &revokedAt, &lastSeenAt, &ip, &ua); err != nil {
		return Session{}, err
	}

	var err error
	s.CreatedAt, err = time.Parse(time.RFC3339Nano, createdAt)
	if err != nil {
		return Session{}, err
	}
	s.ExpiresAt, err = time.Parse(time.RFC3339Nano, expiresAt)
	if err != nil {
		return Session{}, err
	}
	if revokedAt.Valid {
		t, err := time.Parse(time.RFC3339Nano, revokedAt.String)
		if err != nil {
			return Session{}, err
		}
		s.RevokedAt = &t
	}
	if lastSeenAt.Valid {
		t, err := time.Parse(time.RFC3339Nano, lastSeenAt.String)
		if err != nil {
			return Session{}, err
		}
		s.LastSeenAt = &t
	}
	if ip.Valid {
		s.IP = &ip.String
	}
	if ua.Valid {
		s.UserAgent = &ua.String
	}
	return s, nil
}

func TouchSession(ctx context.Context, db *sql.DB, sessionID string) error {
	now := time.Now().UTC().Format(time.RFC3339Nano)
	_, err := db.ExecContext(ctx, `UPDATE sessions SET last_seen_at = ? WHERE id = ?`, now, sessionID)
	return err
}

func RevokeSession(ctx context.Context, db *sql.DB, sessionID string) error {
	now := time.Now().UTC().Format(time.RFC3339Nano)
	_, err := db.ExecContext(ctx, `UPDATE sessions SET revoked_at = ? WHERE id = ?`, now, sessionID)
	return err
}

func DeleteExpiredSessions(ctx context.Context, db *sql.DB) (int64, error) {
	res, err := db.ExecContext(ctx, `DELETE FROM sessions WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now') OR revoked_at IS NOT NULL`)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}
