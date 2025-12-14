package httpapi

import (
	"context"
	"database/sql"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/httpapi/routes"
	"github.com/LukeHagar/Rivet/internal/observability"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/ws"
)

type Deps struct {
	DB *sql.DB

	SessionCookieName string
	SessionTTL        time.Duration

	SecretKeyBase64   string
	AllowInternalURLs bool

	Hub *ws.Hub
	Metrics *observability.Metrics
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
	routes.RegisterMetrics(app, deps.Metrics)
	if deps.Hub != nil {
		routes.RegisterWebSocket(app, deps.Hub)
	}

	api := app.Group("/api/v1")
	routes.RegisterDiagnostics(api, routes.DiagnosticsDeps{DB: deps.DB, Metrics: deps.Metrics})
	routes.RegisterSetup(api, routes.SetupDeps{DB: deps.DB, SetupMode: setupGate.IsSetupMode, Hub: deps.Hub})
	routes.RegisterSetupPaths(api, deps.DB, deps.Hub)
	sb, _ := security.NewSecretBox(deps.SecretKeyBase64)
	policy := security.OutboundPolicy{AllowInternal: deps.AllowInternalURLs}
	routes.RegisterSetupClients(api, routes.SetupClientsDeps{
		DB:        deps.DB,
		SecretBox: sb,
		Policy:    policy,
		Hub:       deps.Hub,
	})
	routes.RegisterSetupIndexers(api, routes.SetupIndexersDeps{
		DB:        deps.DB,
		SecretBox: sb,
		Policy:    policy,
		Hub:       deps.Hub,
	})
	routes.RegisterSetupMetadata(api, routes.SetupMetadataDeps{
		DB:        deps.DB,
		SecretBox: sb,
		Policy:    policy,
		Hub:       deps.Hub,
	})
	routes.RegisterSetupPolicies(api, deps.DB, deps.Hub)
	routes.RegisterSetupFinish(api, deps.DB, deps.Hub)
	routes.RegisterTargetsAndSearch(api, routes.TargetsDeps{
		DB:        deps.DB,
		SecretBox: sb,
		Policy:    policy,
		Hub:       deps.Hub,
		Metrics:   deps.Metrics,
	})
	routes.RegisterAuth(api, routes.AuthDeps{
		DB:                deps.DB,
		SessionCookieName: deps.SessionCookieName,
		SessionTTL:        deps.SessionTTL,
		LoginLimiter:      security.NewRateLimiter(10, 1*time.Minute),
		Hub:               deps.Hub,
	})

	return app, setupGate, nil
}
