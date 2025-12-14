package middleware

import (
	"context"
	"net/http"
	"strings"
	"sync/atomic"
	"time"

	"github.com/gofiber/fiber/v2"
)

type SetupGate struct {
	// setupMode is an atomic hint to avoid a COUNT(*) on every request.
	// It is refreshed periodically.
	setupMode atomic.Bool

	refreshEvery time.Duration
	countUsers    func(ctx context.Context) (int, error)
}

func NewSetupGate(refreshEvery time.Duration, countUsers func(ctx context.Context) (int, error)) *SetupGate {
	g := &SetupGate{refreshEvery: refreshEvery, countUsers: countUsers}
	g.setupMode.Store(true)
	return g
}

func (g *SetupGate) StartRefresher(ctx context.Context) {
	if g.refreshEvery <= 0 {
		g.refreshEvery = 2 * time.Second
	}

	// Initial refresh: avoid a window where setupMode defaults to true.
	if n, err := g.countUsers(ctx); err == nil {
		g.setupMode.Store(n == 0)
	}

	go func() {
		t := time.NewTicker(g.refreshEvery)
		defer t.Stop()

		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				n, err := g.countUsers(ctx)
				if err != nil {
					continue
				}
				g.setupMode.Store(n == 0)
			}
		}
	}()
}

func (g *SetupGate) IsSetupMode() bool {
	return g.setupMode.Load()
}

func (g *SetupGate) Middleware() fiber.Handler {
	return func(c *fiber.Ctx) error {
		if !g.IsSetupMode() {
			return c.Next()
		}

		path := c.Path()
		// Always allow health endpoints.
		if path == "/healthz" || path == "/readyz" || path == "/metrics" {
			return c.Next()
		}

		// Allow setup endpoints.
		if strings.HasPrefix(path, "/api/v1/setup/") {
			return c.Next()
		}

		return c.Status(http.StatusForbidden).JSON(fiber.Map{
			"error":     "setup_required",
			"message":   "setup is required before using the app",
			"setupMode": true,
		})
	}
}
