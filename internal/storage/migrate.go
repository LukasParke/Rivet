package storage

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"fmt"
	"io/fs"
	"sort"
	"strings"
)

type MigrationFS interface {
	ReadDir(name string) ([]fs.DirEntry, error)
	ReadFile(name string) ([]byte, error)
}

func EnsureMigrationsTable(ctx context.Context, db *sql.DB) error {
	_, err := db.ExecContext(ctx, `
CREATE TABLE IF NOT EXISTS schema_migrations (
  id TEXT PRIMARY KEY,
  applied_at TEXT NOT NULL
);
`)
	return err
}

func AppliedMigrations(ctx context.Context, db *sql.DB) (map[string]bool, error) {
	rows, err := db.QueryContext(ctx, `SELECT id FROM schema_migrations`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	m := map[string]bool{}
	for rows.Next() {
		var id string
		if err := rows.Scan(&id); err != nil {
			return nil, err
		}
		m[id] = true
	}
	return m, rows.Err()
}

func ApplyMigrations(ctx context.Context, db *sql.DB, mfs MigrationFS, dir string) error {
	if err := EnsureMigrationsTable(ctx, db); err != nil {
		return err
	}

	applied, err := AppliedMigrations(ctx, db)
	if err != nil {
		return err
	}

	entries, err := mfs.ReadDir(dir)
	if err != nil {
		return err
	}

	var files []string
	for _, e := range entries {
		if e.IsDir() {
			continue
		}
		name := e.Name()
		if strings.HasSuffix(name, ".sql") {
			files = append(files, name)
		}
	}
	sort.Strings(files)

	for _, name := range files {
		path := dir + "/" + name
		b, err := mfs.ReadFile(path)
		if err != nil {
			return err
		}

		h := sha256.Sum256(b)
		id := name + ":" + hex.EncodeToString(h[:])
		if applied[id] {
			continue
		}

		if _, err := db.ExecContext(ctx, string(b)); err != nil {
			return fmt.Errorf("apply migration %s: %w", name, err)
		}

		if _, err := db.ExecContext(ctx, `INSERT INTO schema_migrations(id, applied_at) VALUES (?, strftime('%Y-%m-%dT%H:%M:%fZ','now'))`, id); err != nil {
			return fmt.Errorf("record migration %s: %w", name, err)
		}
	}

	return nil
}
