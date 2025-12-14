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

func CategoryFromType(t string) string {
	for i := 0; i < len(t); i++ {
		if t[i] == '.' {
			return t[:i]
		}
	}
	return t
}
