package routes

import (
	"context"
	"time"

	"github.com/gofiber/fiber/v2"
)

type DBPinger interface {
	PingContext(ctx context.Context) error
}

func RegisterHealth(app *fiber.App, db DBPinger) {
	app.Get("/healthz", func(c *fiber.Ctx) error {
		return c.JSON(fiber.Map{"ok": true})
	})

	app.Get("/readyz", func(c *fiber.Ctx) error {
		ctx, cancel := context.WithTimeout(c.UserContext(), 2*time.Second)
		defer cancel()
		if err := db.PingContext(ctx); err != nil {
			return c.Status(503).JSON(fiber.Map{"ok": false, "error": "db_unready"})
		}
		return c.JSON(fiber.Map{"ok": true})
	})
}
