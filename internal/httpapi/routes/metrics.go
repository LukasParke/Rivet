package routes

import (
	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/observability"
)

func RegisterMetrics(app *fiber.App, m *observability.Metrics) {
	app.Get("/metrics", func(c *fiber.Ctx) error {
		if m == nil {
			return c.SendString("")
		}
		c.Set("content-type", "text/plain; version=0.0.4")
		return c.SendString(m.RenderText())
	})
}
