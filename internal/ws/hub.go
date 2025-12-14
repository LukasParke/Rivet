package ws

import (
	"sync"
)

type Hub struct {
	mu    sync.RWMutex
	conns map[*Conn]struct{}
}

func NewHub() *Hub {
	return &Hub{conns: map[*Conn]struct{}{}}
}

func (h *Hub) Add(c *Conn) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.conns[c] = struct{}{}
}

func (h *Hub) Remove(c *Conn) {
	h.mu.Lock()
	defer h.mu.Unlock()
	delete(h.conns, c)
}

func (h *Hub) Publish(eventType string, payload any) {
	e := NewEnvelope(eventType, payload)
	b, err := MarshalEnvelope(e)
	if err != nil {
		return
	}
	cat := CategoryFromType(eventType)

	h.mu.RLock()
	defer h.mu.RUnlock()
	for c := range h.conns {
		c.TrySend(cat, b)
	}
}
