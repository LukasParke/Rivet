package routes

import (
	"context"
	"database/sql"
	"net/http"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type AcquisitionsDeps struct {
	DB *sql.DB
}

func RegisterAcquisitions(api fiber.Router, deps AcquisitionsDeps) {
	api.Get("/acquisitions", middleware.RequireRole("operator"), func(c *fiber.Ctx) error {
		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()
		items, err := storage.ListAcquisitions(ctx, deps.DB, 50)
		if err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
		}
		return c.JSON(items)
	})
}
