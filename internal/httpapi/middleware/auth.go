package middleware

import (
	"context"
	"database/sql"
	"net/http"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/storage"
)

type ctxKey string

const (
	ctxUserKey    ctxKey = "user"
	ctxSessionKey ctxKey = "session"
)

type Auth struct {
	DB                *sql.DB
	SessionCookieName string
}

func NewAuth(db *sql.DB, sessionCookieName string) *Auth {
	return &Auth{DB: db, SessionCookieName: sessionCookieName}
}

func UserFromContext(c *fiber.Ctx) (storage.User, bool) {
	u, ok := c.UserContext().Value(ctxUserKey).(storage.User)
	return u, ok
}

func SessionFromContext(c *fiber.Ctx) (storage.Session, bool) {
	s, ok := c.UserContext().Value(ctxSessionKey).(storage.Session)
	return s, ok
}

// LoadSession loads a session from a cookie and attaches user + session to context.
// Missing/invalid sessions are treated as anonymous.
func (a *Auth) LoadSession() fiber.Handler {
	return func(c *fiber.Ctx) error {
		sid := strings.TrimSpace(c.Cookies(a.SessionCookieName))
		if sid == "" {
			return c.Next()
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 2*time.Second)
		defer cancel()

		s, err := storage.GetSession(ctx, a.DB, sid)
		if err != nil {
			// Anonymous on lookup errors.
			return c.Next()
		}
		if s.RevokedAt != nil || time.Now().UTC().After(s.ExpiresAt) {
			return c.Next()
		}

		u, err := storage.GetUserByID(ctx, a.DB, s.UserID)
		if err != nil {
			return c.Next()
		}

		_ = storage.TouchSession(ctx, a.DB, s.ID)

		uc := context.WithValue(c.UserContext(), ctxSessionKey, s)
		uc = context.WithValue(uc, ctxUserKey, u)
		c.SetUserContext(uc)

		// Also set locals for WebSocket upgrade handlers.
		c.Locals("session", s)
		c.Locals("user", u)

		return c.Next()
	}
}

func RequireAuth() fiber.Handler {
	return func(c *fiber.Ctx) error {
		if _, ok := UserFromContext(c); !ok {
			return c.Status(http.StatusUnauthorized).JSON(fiber.Map{"error": "unauthorized"})
		}
		return c.Next()
	}
}

// RequireRole requires the user to have at least the provided role.
// Role order: viewer < operator < admin.
func RequireRole(minRole string) fiber.Handler {
	min := roleRank(minRole)
	return func(c *fiber.Ctx) error {
		u, ok := UserFromContext(c)
		if !ok {
			return c.Status(http.StatusUnauthorized).JSON(fiber.Map{"error": "unauthorized"})
		}
		if roleRank(u.Role) < min {
			return c.Status(http.StatusForbidden).JSON(fiber.Map{"error": "forbidden"})
		}
		return c.Next()
	}
}

func roleRank(role string) int {
	switch strings.ToLower(strings.TrimSpace(role)) {
	case "admin":
		return 3
	case "operator":
		return 2
	case "viewer":
		return 1
	default:
		return 0
	}
}

// CSRFMiddleware enforces double-submit CSRF on cookie-authenticated mutations.
// It only triggers when a valid session is present.
func CSRFMiddleware() fiber.Handler {
	unsafe := map[string]bool{"POST": true, "PUT": true, "PATCH": true, "DELETE": true}
	return func(c *fiber.Ctx) error {
		if !unsafe[c.Method()] {
			return c.Next()
		}
		s, ok := SessionFromContext(c)
		if !ok {
			return c.Next()
		}
		h := strings.TrimSpace(c.Get("X-CSRF-Token"))
		if h == "" || s.CSRFToken == "" || h != s.CSRFToken {
			return c.Status(http.StatusForbidden).JSON(fiber.Map{"error": "csrf_required"})
		}
		return c.Next()
	}
}
