package storage

import (
	"context"
	"database/sql"
	"time"
)

func GetUserByEmail(ctx context.Context, db *sql.DB, email string) (User, error) {
	var u User
	var totp sql.NullString
	var createdAt, updatedAt string

	row := db.QueryRowContext(ctx, `
SELECT id, email, password_hash, role, totp_secret_enc, created_at, updated_at
FROM users
WHERE email = ?
`, email)
	if err := row.Scan(&u.ID, &u.Email, &u.PasswordHash, &u.Role, &totp, &createdAt, &updatedAt); err != nil {
		return User{}, err
	}
	if totp.Valid {
		u.TOTPSecretEnc = &totp.String
	}
	var err error
	u.CreatedAt, err = time.Parse(time.RFC3339Nano, createdAt)
	if err != nil {
		return User{}, err
	}
	u.UpdatedAt, err = time.Parse(time.RFC3339Nano, updatedAt)
	if err != nil {
		return User{}, err
	}
	return u, nil
}

func GetUserByID(ctx context.Context, db *sql.DB, id string) (User, error) {
	var u User
	var totp sql.NullString
	var createdAt, updatedAt string

	row := db.QueryRowContext(ctx, `
SELECT id, email, password_hash, role, totp_secret_enc, created_at, updated_at
FROM users
WHERE id = ?
`, id)
	if err := row.Scan(&u.ID, &u.Email, &u.PasswordHash, &u.Role, &totp, &createdAt, &updatedAt); err != nil {
		return User{}, err
	}
	if totp.Valid {
		u.TOTPSecretEnc = &totp.String
	}
	var err error
	u.CreatedAt, err = time.Parse(time.RFC3339Nano, createdAt)
	if err != nil {
		return User{}, err
	}
	u.UpdatedAt, err = time.Parse(time.RFC3339Nano, updatedAt)
	if err != nil {
		return User{}, err
	}
	return u, nil
}
