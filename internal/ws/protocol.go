package ws

import (
	"encoding/json"
	"strings"
)

type ClientMessage struct {
	Type      string   `json:"type"`
	Subscribe []string `json:"subscribe"`
	Unsub     []string `json:"unsubscribe"`
}

func ApplyClientMessage(c *Conn, b []byte) {
	var m ClientMessage
	if err := json.Unmarshal(b, &m); err != nil {
		return
	}

	if m.Type == "subscribe" {
		for _, s := range m.Subscribe {
			s = strings.TrimSpace(s)
			if s != "" {
				c.Subscribe(s)
			}
		}
	}
	if m.Type == "unsubscribe" {
		for _, s := range m.Unsub {
			s = strings.TrimSpace(s)
			if s != "" {
				c.Unsubscribe(s)
			}
		}
	}
}
