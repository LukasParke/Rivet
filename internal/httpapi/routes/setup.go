package routes

import (
	"context"
	"database/sql"
	"net/http"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/ws"
)

type SetupDeps struct {
	DB *sql.DB
	SetupMode func() bool
	Hub *ws.Hub
}

type SetupStatusResponse struct {
	SetupMode bool `json:"setupMode"`
}

type CreateAdminRequest struct {
	Email    string `json:"email"`
	Password string `json:"password"`
}

type CreateAdminResponse struct {
	UserID string `json:"userId"`
}

func RegisterSetup(app fiber.Router, deps SetupDeps) {
	app.Get("/setup/status", func(c *fiber.Ctx) error {
		return c.JSON(SetupStatusResponse{SetupMode: deps.SetupMode()})
	})

	app.Post("/setup/admin", func(c *fiber.Ctx) error {
		if !deps.SetupMode() {
			return c.Status(http.StatusConflict).JSON(fiber.Map{
				"error":   "setup_complete",
				"message": "setup has already been completed",
			})
		}

		var req CreateAdminRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}
		req.Email = strings.TrimSpace(req.Email)
		req.Password = strings.TrimSpace(req.Password)
		if req.Email == "" || !strings.Contains(req.Email, "@") {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_email"})
		}

		h, err := security.HashPassword(req.Password)
		if err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_password", "message": err.Error()})
		}

		// Double-check within request to avoid race.
		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()
		n, err := storage.CountUsers(ctx, deps.DB)
		if err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "db_error"})
		}
		if n != 0 {
			return c.Status(http.StatusConflict).JSON(fiber.Map{"error": "setup_complete"})
		}

		u, err := storage.CreateUser(ctx, deps.DB, req.Email, h.Encoded, "admin")
		if err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "create_user_failed"})
		}

		_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{Action: "setup.admin_created", ActorUserID: &u.ID})
		emitAudit(deps.Hub, "setup.admin_created", &u.ID, nil)

		return c.Status(http.StatusCreated).JSON(CreateAdminResponse{UserID: u.ID})
	})
}
