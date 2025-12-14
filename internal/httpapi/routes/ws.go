package routes

import (
	"net/http"

	"github.com/gofiber/fiber/v2"
	"github.com/gofiber/websocket/v2"

	"github.com/LukeHagar/Rivet/internal/ws"
)

func RegisterWebSocket(app *fiber.App, hub *ws.Hub) {
	app.Get("/ws", websocket.New(func(c *websocket.Conn) {
		// Auth: requires session middleware to have populated locals.
		if c.Locals("user") == nil {
			_ = c.WriteMessage(websocket.TextMessage, []byte(`{"v":1,"type":"error","ts":"","payload":{"error":"unauthorized"}}`))
			_ = c.Close()
			return
		}

		conn := ws.NewConn(c)
		hub.Add(conn)
		defer func() {
			hub.Remove(conn)
			conn.Close()
			_ = c.Close()
		}()

		// Default channels
		conn.Subscribe("activity")
		conn.Subscribe("jobs")
		conn.Subscribe("audit")

		// Send hello to this connection only
		hello, _ := ws.MarshalEnvelope(ws.NewEnvelope("ws.connected", fiber.Map{"ok": true, "channels": []string{"activity", "jobs", "audit"}}))
		_ = c.WriteMessage(websocket.TextMessage, hello)

		// Writer loop
		go conn.WriteLoop()

		for {
			mt, msg, err := c.ReadMessage()
			if err != nil {
				return
			}
			if mt != websocket.TextMessage {
				continue
			}
			ws.ApplyClientMessage(conn, msg)
		}
	}))

	// Fiber websocket middleware uses 426 upgrade response automatically; keep an explicit route to avoid 404.
	app.Get("/ws/info", func(c *fiber.Ctx) error {
		return c.Status(http.StatusOK).JSON(fiber.Map{"ok": true})
	})
}
