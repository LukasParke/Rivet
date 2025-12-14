package main

import (
	"context"
	"log/slog"
	"os"
	"time"

	"github.com/LukeHagar/Rivet/internal/config"
	"github.com/LukeHagar/Rivet/internal/observability"
	"github.com/LukeHagar/Rivet/internal/storage"
)

func main() {
	cfg, err := config.Load()
	if err != nil {
		panic(err)
	}
	log := observability.NewLogger(cfg.Env)

	db, err := storage.OpenSQLite(cfg.DatabaseURL)
	if err != nil {
		log.Error("open db", slog.Any("err", err))
		os.Exit(1)
	}
	defer func() { _ = db.Close() }()

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	if err := storage.ApplyMigrations(ctx, db.SQL, storage.MigrationsFS(), "migrations"); err != nil {
		log.Error("apply migrations", slog.Any("err", err))
		os.Exit(1)
	}

	log.Info("migrations applied")
}
