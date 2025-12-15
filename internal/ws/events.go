package ws

import (
	"encoding/json"
	"time"
)

type Envelope struct {
	V       int         `json:"v"`
	Type    string      `json:"type"`
	TS      string      `json:"ts"`
	Payload any         `json:"payload"`
}

func NewEnvelope(eventType string, payload any) Envelope {
	return Envelope{
		V:       1,
		Type:    eventType,
		TS:      time.Now().UTC().Format(time.RFC3339Nano),
		Payload: payload,
	}
}

func MarshalEnvelope(e Envelope) ([]byte, error) {
	return json.Marshal(e)
}

func ChannelsForType(t string) []string {
	// Always include activity for a general feed.
	ch := []string{"activity"}
	switch prefix(t) {
	case "job":
		return append(ch, "jobs")
	case "search", "candidate", "decision":
		return append(ch, "jobs")
	case "audit":
		return append(ch, "audit")
	case "health":
		return append(ch, "health")
	case "verify":
		return append(ch, "imports")
	case "import":
		return append(ch, "imports")
	case "download":
		return append(ch, "downloads")
	default:
		return ch
	}
}

func prefix(t string) string {
	for i := 0; i < len(t); i++ {
		if t[i] == '.' {
			return t[:i]
		}
	}
	return t
}
