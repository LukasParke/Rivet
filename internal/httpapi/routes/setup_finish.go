package routes

import (
	"context"
	"database/sql"
	"encoding/json"
	"net/http"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/ws"
)

type SetupFinishResponse struct {
	Ok        bool              `json:"ok"`
	ReportKey string            `json:"reportKey"`
	Summary   map[string]string `json:"summary"`
}

func RegisterSetupFinish(api fiber.Router, db *sql.DB, hub *ws.Hub) {
	grp := api.Group("/setup")

	grp.Post("/finish", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		ctx, cancel := context.WithTimeout(c.UserContext(), 8*time.Second)
		defer cancel()

		// Store a basic setup report snapshot.
		summary := map[string]string{
			"finishedAt": time.Now().UTC().Format(time.RFC3339Nano),
		}
		for _, k := range []string{"setup.paths", "setup.clients", "setup.indexers", "setup.metadata", "setup.policies"} {
			_, ok, err := storage.GetSetting(ctx, db, k)
			if err != nil {
				summary[k] = "error"
				continue
			}
			if ok {
				summary[k] = "set"
			} else {
				summary[k] = "missing"
			}
		}

		b, _ := json.Marshal(summary)
		reportKey := "setup.report.latest"
		if err := storage.UpsertSetting(ctx, db, reportKey, string(b)); err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "save_failed"})
		}

		// Close registration by default (future invites feature can flip it).
		_ = storage.UpsertSetting(ctx, db, "auth.registration_open", `false`)
		var actor *string
		if u, ok := middleware.UserFromContext(c); ok {
			actor = &u.ID
		}
		_ = storage.InsertAuditEvent(ctx, db, storage.AuditEvent{Action: "setup.finished", ActorUserID: actor})
		emitAudit(hub, "setup.finished", actor, map[string]any{"reportKey": reportKey})

		return c.JSON(SetupFinishResponse{Ok: true, ReportKey: reportKey, Summary: summary})
	})
}
