package security

import (
	"sync"
	"time"
)

// Simple in-memory fixed-window rate limiter.
// Good enough for single-node dev/MVP; Redis-backed limiter comes later.

type RateLimiter struct {
	mu     sync.Mutex
	window time.Duration
	limit  int
	m      map[string]entry
}

type entry struct {
	resetAt time.Time
	count   int
}

func NewRateLimiter(limit int, window time.Duration) *RateLimiter {
	return &RateLimiter{window: window, limit: limit, m: map[string]entry{}}
}

func (r *RateLimiter) Allow(key string, now time.Time) bool {
	r.mu.Lock()
	defer r.mu.Unlock()

	e, ok := r.m[key]
	if !ok || now.After(e.resetAt) {
		r.m[key] = entry{resetAt: now.Add(r.window), count: 1}
		return true
	}
	if e.count >= r.limit {
		return false
	}
	e.count++
	r.m[key] = e
	return true
}
