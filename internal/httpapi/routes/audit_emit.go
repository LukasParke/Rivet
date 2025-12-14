package routes

import (
	"github.com/LukeHagar/Rivet/internal/ws"
)

func emitAudit(hub *ws.Hub, action string, actorUserID *string, extra map[string]any) {
	if hub == nil {
		return
	}
	p := map[string]any{"action": action}
	if actorUserID != nil {
		p["actorUserId"] = *actorUserID
	}
	for k, v := range extra {
		p[k] = v
	}
	hub.Publish("audit.event", p)
}
