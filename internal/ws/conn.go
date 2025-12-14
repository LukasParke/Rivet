package ws

import (
	"sync"

	"github.com/gofiber/websocket/v2"
)

type Conn struct {
	ws *websocket.Conn

	send chan []byte

	mu           sync.RWMutex
	subscriptions map[string]bool
}

func NewConn(ws *websocket.Conn) *Conn {
	return &Conn{
		ws:           ws,
		send:         make(chan []byte, 128),
		subscriptions: map[string]bool{},
	}
}

func (c *Conn) Subscribe(cat string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.subscriptions[cat] = true
}

func (c *Conn) Unsubscribe(cat string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	delete(c.subscriptions, cat)
}

func (c *Conn) Subscribed(cat string) bool {
	c.mu.RLock()
	defer c.mu.RUnlock()
	// If no subscriptions set, treat as subscribe-all.
	if len(c.subscriptions) == 0 {
		return true
	}
	return c.subscriptions[cat]
}

func (c *Conn) TrySend(channels []string, b []byte) {
	if len(channels) == 0 {
		channels = []string{"activity"}
	}
	ok := false
	for _, ch := range channels {
		if c.Subscribed(ch) {
			ok = true
			break
		}
	}
	if !ok {
		return
	}
	select {
	case c.send <- b:
	default:
		// drop when client is slow
	}
}

func (c *Conn) WriteLoop() {
	for msg := range c.send {
		_ = c.ws.WriteMessage(websocket.TextMessage, msg)
	}
}

func (c *Conn) Close() {
	close(c.send)
}
