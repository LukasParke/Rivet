package ws

import (
	"encoding/json"
	"strings"
)

type ClientMessage struct {
	Type      string   `json:"type"`
	Channels  []string `json:"channels"`
	Subscribe []string `json:"subscribe"`
	Unsub     []string `json:"unsubscribe"`
}

func ApplyClientMessage(c *Conn, b []byte) {
	var m ClientMessage
	if err := json.Unmarshal(b, &m); err != nil {
		return
	}

	if m.Type == "subscribe" {
		subs := m.Subscribe
		if len(m.Channels) > 0 {
			subs = m.Channels
		}
		for _, s := range subs {
			s = strings.TrimSpace(s)
			if s != "" {
				c.Subscribe(s)
			}
		}
	}
	if m.Type == "unsubscribe" {
		unsubs := m.Unsub
		if len(m.Channels) > 0 {
			unsubs = m.Channels
		}
		for _, s := range unsubs {
			s = strings.TrimSpace(s)
			if s != "" {
				c.Unsubscribe(s)
			}
		}
	}
}
