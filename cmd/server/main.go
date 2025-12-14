package main

import (
	"context"
	"errors"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/LukeHagar/Rivet/internal/config"
	"github.com/LukeHagar/Rivet/internal/httpapi"
	"github.com/LukeHagar/Rivet/internal/observability"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

func main() {
	cfg, err := config.Load()
	if err != nil {
		panic(err)
	}
	log := observability.NewLogger(cfg.Env)

	// Initialize SecretBox early to validate configuration.
	if _, err := security.NewSecretBox(cfg.SecretKeyBase64); err != nil {
		log.Error("invalid secret key", slog.Any("err", err))
		os.Exit(1)
	}

	db, err := storage.OpenSQLite(cfg.DatabaseURL)
	if err != nil {
		log.Error("open db", slog.Any("err", err))
		os.Exit(1)
	}
	defer func() { _ = db.Close() }()

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	if err := storage.ApplyMigrations(ctx, db.SQL, storage.MigrationsFS(), "migrations"); err != nil {
		log.Error("apply migrations", slog.Any("err", err))
		os.Exit(1)
	}

	app, _, err := httpapi.NewApp(ctx, httpapi.Deps{
		DB:                db.SQL,
		SessionCookieName: cfg.SessionCookieName,
		SessionTTL:        cfg.SessionTTL,
	})
	if err != nil {
		log.Error("build app", slog.Any("err", err))
		os.Exit(1)
	}

	errCh := make(chan error, 1)
	go func() {
		log.Info("listening", slog.String("addr", cfg.HTTPAddr))
		errCh <- app.Listen(cfg.HTTPAddr)
	}()

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	select {
	case sig := <-sigCh:
		log.Info("shutdown signal", slog.String("signal", sig.String()))
	case err := <-errCh:
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Error("server error", slog.Any("err", err))
		}
	}

	shutdownCtx, shutdownCancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer shutdownCancel()
	if err := app.ShutdownWithContext(shutdownCtx); err != nil {
		log.Error("shutdown", slog.Any("err", err))
	}
}
