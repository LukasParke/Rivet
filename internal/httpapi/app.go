package httpapi

import (
	"context"
	"database/sql"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/httpapi/routes"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type Deps struct {
	DB *sql.DB
}

func NewApp(ctx context.Context, deps Deps) (*fiber.App, *middleware.SetupGate, error) {
	app := fiber.New(fiber.Config{
		DisableStartupMessage: true,
		ReadTimeout:           15 * time.Second,
		WriteTimeout:          15 * time.Second,
		IdleTimeout:           60 * time.Second,
	})

	setupGate := middleware.NewSetupGate(2*time.Second, func(ctx context.Context) (int, error) {
		// Use a short timeout for the refresher.
		c, cancel := context.WithTimeout(ctx, 1*time.Second)
		defer cancel()
		return storage.CountUsers(c, deps.DB)
	})
	setupGate.StartRefresher(ctx)

	app.Use(setupGate.Middleware())

	routes.RegisterHealth(app, deps.DB)

	api := app.Group("/api/v1")
	routes.RegisterSetup(api, routes.SetupDeps{DB: deps.DB, SetupMode: setupGate.IsSetupMode})

	return app, setupGate, nil
}
