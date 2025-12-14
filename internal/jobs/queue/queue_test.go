package queue

import (
	"math/rand"
	"testing"
	"time"
)

func TestNextBackoffIncreases(t *testing.T) {
	rand.Seed(1)
	now := time.Unix(0, 0).UTC()
	a1 := nextBackoff(1, now)
	a2 := nextBackoff(2, now)

	// attempt=1 => base=2s, jitter in [0..1s]
	if a1.Before(now.Add(2*time.Second)) || a1.After(now.Add(3*time.Second)) {
		t.Fatalf("attempt1 out of range: %v", a1.Sub(now))
	}
	// attempt=2 => base=4s, jitter in [0..2s]
	if a2.Before(now.Add(4*time.Second)) || a2.After(now.Add(6*time.Second)) {
		t.Fatalf("attempt2 out of range: %v", a2.Sub(now))
	}
}
