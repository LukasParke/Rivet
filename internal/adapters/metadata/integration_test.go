package metadata_test

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/LukeHagar/Rivet/internal/adapters/metadata/tmdb"
	"github.com/LukeHagar/Rivet/internal/adapters/metadata/tvdb"
	"github.com/LukeHagar/Rivet/internal/security"
)

func TestIntegration_TMDB_Validate(t *testing.T) {
	key := os.Getenv("TMDB_API_KEY")
	if key == "" {
		t.Skip("TMDB_API_KEY not set")
	}

	c, err := tmdb.New("https://api.themoviedb.org", key, security.OutboundPolicy{AllowInternal: false})
	if err != nil {
		t.Fatalf("new: %v", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	_, err = c.Validate(ctx)
	if err != nil {
		t.Fatalf("validate: %v", err)
	}
}

func TestIntegration_TVDB_Validate(t *testing.T) {
	key := os.Getenv("TVDB_API_KEY")
	if key == "" {
		t.Skip("TVDB_API_KEY not set")
	}

	c, err := tvdb.New("https://api4.thetvdb.com", key, security.OutboundPolicy{AllowInternal: false})
	if err != nil {
		t.Fatalf("new: %v", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	_, err = c.Validate(ctx)
	if err != nil {
		t.Fatalf("validate: %v", err)
	}
}
