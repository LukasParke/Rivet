package routes

import (
	"context"
	"database/sql"
	"net/http"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/ws"
)

type AuthDeps struct {
	DB                *sql.DB
	SessionCookieName string
	SessionTTL        time.Duration
	LoginLimiter      *security.RateLimiter
	Hub               *ws.Hub
}

type LoginRequest struct {
	Email    string `json:"email"`
	Password string `json:"password"`
}

type MeResponse struct {
	UserID string `json:"userId"`
	Email  string `json:"email"`
	Role   string `json:"role"`
}

type CSRFResponse struct {
	Token string `json:"token"`
}

func RegisterAuth(api fiber.Router, deps AuthDeps) {
	api.Get("/auth/csrf", middleware.RequireAuth(), func(c *fiber.Ctx) error {
		s, ok := middleware.SessionFromContext(c)
		if !ok || s.CSRFToken == "" {
			return c.Status(http.StatusUnauthorized).JSON(fiber.Map{"error": "unauthorized"})
		}
		return c.JSON(CSRFResponse{Token: s.CSRFToken})
	})

	api.Post("/auth/login", func(c *fiber.Ctx) error {
		ip := strings.TrimSpace(c.IP())
		if deps.LoginLimiter != nil {
			if !deps.LoginLimiter.Allow("login:"+ip, time.Now()) {
				return c.Status(http.StatusTooManyRequests).JSON(fiber.Map{"error": "rate_limited"})
			}
		}

		var req LoginRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}
		email := strings.TrimSpace(strings.ToLower(req.Email))
		pw := strings.TrimSpace(req.Password)
		if email == "" || !strings.Contains(email, "@") {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_email"})
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()

		u, err := storage.GetUserByEmail(ctx, deps.DB, email)
		if err != nil {
			// Avoid user enumeration.
			return c.Status(http.StatusUnauthorized).JSON(fiber.Map{"error": "invalid_credentials"})
		}
		ok, err := security.VerifyPassword(pw, u.PasswordHash)
		if err != nil || !ok {
			return c.Status(http.StatusUnauthorized).JSON(fiber.Map{"error": "invalid_credentials"})
		}

		ua := strings.TrimSpace(c.Get("User-Agent"))
		var uaPtr *string
		if ua != "" {
			uaPtr = &ua
		}
		var ipPtr *string
		if ip != "" {
			ipPtr = &ip
		}

		sess, err := storage.CreateSession(ctx, deps.DB, u.ID, deps.SessionTTL, ipPtr, uaPtr)
		if err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "session_create_failed"})
		}

		// HttpOnly cookie, secure should be set by reverse proxy/TLS termination.
		c.Cookie(&fiber.Cookie{
			Name:     deps.SessionCookieName,
			Value:    sess.ID,
			HTTPOnly: true,
			SameSite: "Lax",
			Path:     "/",
			Expires:  sess.ExpiresAt,
		})

		_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{ActorUserID: &u.ID, Action: "auth.login", IP: ipPtr, UserAgent: uaPtr})
		emitAudit(deps.Hub, "auth.login", &u.ID, map[string]any{"ip": ip, "userAgent": ua})

		// Return CSRF token to client to store and attach as X-CSRF-Token on mutations.
		return c.JSON(CSRFResponse{Token: sess.CSRFToken})
	})

	api.Post("/auth/logout", func(c *fiber.Ctx) error {
		// If no session, treat as idempotent.
		sid := strings.TrimSpace(c.Cookies(deps.SessionCookieName))
		if sid == "" {
			return c.SendStatus(http.StatusNoContent)
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 5*time.Second)
		defer cancel()

		s, err := storage.GetSession(ctx, deps.DB, sid)
		if err == nil {
			_ = storage.RevokeSession(ctx, deps.DB, sid)
			ip := strings.TrimSpace(c.IP())
			ua := strings.TrimSpace(c.Get("User-Agent"))
			var ipPtr *string
			var uaPtr *string
			if ip != "" {
				ipPtr = &ip
			}
			if ua != "" {
				uaPtr = &ua
			}
			_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{ActorUserID: &s.UserID, Action: "auth.logout", IP: ipPtr, UserAgent: uaPtr})
			emitAudit(deps.Hub, "auth.logout", &s.UserID, map[string]any{"ip": ip, "userAgent": ua})
		}

		c.Cookie(&fiber.Cookie{
			Name:     deps.SessionCookieName,
			Value:    "",
			HTTPOnly: true,
			SameSite: "Lax",
			Path:     "/",
			Expires:  time.Unix(0, 0),
		})
		return c.SendStatus(http.StatusNoContent)
	})

	api.Get("/auth/me", middleware.RequireAuth(), func(c *fiber.Ctx) error {
		u, ok := middleware.UserFromContext(c)
		if !ok {
			return c.Status(http.StatusUnauthorized).JSON(fiber.Map{"error": "unauthorized"})
		}
		return c.JSON(MeResponse{UserID: u.ID, Email: u.Email, Role: u.Role})
	})
}
