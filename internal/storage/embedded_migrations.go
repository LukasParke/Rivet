package storage

import "embed"

//go:embed migrations/*.sql
var migrationsFS embed.FS

func MigrationsFS() MigrationFS {
	return migrationsFS
}
