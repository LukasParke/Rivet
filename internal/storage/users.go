package storage

import (
	"context"
	"database/sql"
	"errors"
	"strings"
	"time"

	"github.com/google/uuid"
)

type User struct {
	ID           string
	Email        string
	PasswordHash string
	Role         string
	TOTPSecretEnc *string
	CreatedAt    time.Time
	UpdatedAt    time.Time
}

func CountUsers(ctx context.Context, db *sql.DB) (int, error) {
	var n int
	if err := db.QueryRowContext(ctx, `SELECT COUNT(1) FROM users`).Scan(&n); err != nil {
		return 0, err
	}
	return n, nil
}

func CreateUser(ctx context.Context, db *sql.DB, email, passwordHash, role string) (User, error) {
	email = strings.TrimSpace(strings.ToLower(email))
	if email == "" {
		return User{}, errors.New("email required")
	}
	if role == "" {
		return User{}, errors.New("role required")
	}

	now := time.Now().UTC()
	u := User{
		ID:           uuid.NewString(),
		Email:        email,
		PasswordHash: passwordHash,
		Role:         role,
		CreatedAt:    now,
		UpdatedAt:    now,
	}

	_, err := db.ExecContext(ctx, `
INSERT INTO users (id, email, password_hash, role, totp_secret_enc, created_at, updated_at)
VALUES (?, ?, ?, ?, NULL, ?, ?)
`, u.ID, u.Email, u.PasswordHash, u.Role, now.Format(time.RFC3339Nano), now.Format(time.RFC3339Nano))
	if err != nil {
		return User{}, err
	}
	return u, nil
}
