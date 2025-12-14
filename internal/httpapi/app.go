package httpapi

import (
	"context"
	"database/sql"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/httpapi/routes"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type Deps struct {
	DB *sql.DB

	SessionCookieName string
	SessionTTL        time.Duration
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

	auth := middleware.NewAuth(deps.DB, deps.SessionCookieName)
	app.Use(auth.LoadSession())
	app.Use(middleware.CSRFMiddleware())

	routes.RegisterHealth(app, deps.DB)

	api := app.Group("/api/v1")
	routes.RegisterSetup(api, routes.SetupDeps{DB: deps.DB, SetupMode: setupGate.IsSetupMode})
	routes.RegisterSetupPaths(api, deps.DB)
	routes.RegisterAuth(api, routes.AuthDeps{
		DB:                deps.DB,
		SessionCookieName: deps.SessionCookieName,
		SessionTTL:        deps.SessionTTL,
		LoginLimiter:      security.NewRateLimiter(10, 1*time.Minute),
	})

	return app, setupGate, nil
}
