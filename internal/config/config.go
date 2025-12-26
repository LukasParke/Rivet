package config

import (
	"errors"
	"os"
	"strconv"
	"strings"
	"time"
)

type Config struct {
	Env string

	HTTPAddr string

	DatabaseURL string

	// DryRun enables a mode intended for local testing where external download clients
	// and filesystem writes are simulated. Network calls to indexers/metadata remain real.
	DryRun bool
	// DryRunFixturePath optionally points to a local file/dir used for verify/import in dry run.
	// This is read-only; no filesystem writes should be performed in dry run.
	DryRunFixturePath string

	// FFProbePath optionally points to ffprobe for deeper media inspection.
	// If empty, verification will try to find ffprobe on PATH.
	FFProbePath string

	// SecretKey is used for encrypting secrets at rest.
	// It must be exactly 32 bytes after base64 decode.
	SecretKeyBase64 string

	SessionCookieName string
	SessionTTL        time.Duration

	SetupMode bool

	// AllowInternalURLs controls SSRF protections for integration endpoints.
	// When false, private/loopback/link-local IPs are blocked for outbound integration calls.
	AllowInternalURLs bool
}

func Load() (Config, error) {
	cfg := Config{}

	cfg.Env = strings.TrimSpace(getenvDefault("APP_ENV", "dev"))
	cfg.HTTPAddr = strings.TrimSpace(getenvDefault("HTTP_ADDR", ":8080"))
	cfg.DatabaseURL = strings.TrimSpace(getenvDefault("DATABASE_URL", "file:/data/app.db?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)"))

	cfg.DryRun = getenvBool("DRY_RUN", false)
	cfg.DryRunFixturePath = strings.TrimSpace(os.Getenv("DRY_RUN_FIXTURE_PATH"))
	cfg.FFProbePath = strings.TrimSpace(os.Getenv("FFPROBE_PATH"))

	cfg.SecretKeyBase64 = strings.TrimSpace(os.Getenv("APP_SECRET_KEY"))

	cfg.SessionCookieName = strings.TrimSpace(getenvDefault("SESSION_COOKIE_NAME", "rivet_session"))
	cfg.SessionTTL = getenvDuration("SESSION_TTL", 30*24*time.Hour)

	cfg.SetupMode = getenvBool("SETUP_MODE", false)
	cfg.AllowInternalURLs = getenvBool("ALLOW_INTERNAL_URLS", cfg.Env == "dev")

	if cfg.SecretKeyBase64 == "" {
		// We allow empty in dev to get started, but it must be set for real deployments.
		if cfg.Env != "dev" {
			return Config{}, errors.New("APP_SECRET_KEY must be set (base64, 32 bytes decoded)")
		}
	}

	return cfg, nil
}

func getenvDefault(k, def string) string {
	v := os.Getenv(k)
	if strings.TrimSpace(v) == "" {
		return def
	}
	return v
}

func getenvBool(k string, def bool) bool {
	v := strings.TrimSpace(os.Getenv(k))
	if v == "" {
		return def
	}
	switch strings.ToLower(v) {
	case "1", "true", "t", "yes", "y", "on":
		return true
	case "0", "false", "f", "no", "n", "off":
		return false
	default:
		return def
	}
}

func getenvDuration(k string, def time.Duration) time.Duration {
	v := strings.TrimSpace(os.Getenv(k))
	if v == "" {
		return def
	}
	if d, err := time.ParseDuration(v); err == nil {
		return d
	}
	// Support plain seconds too.
	if n, err := strconv.Atoi(v); err == nil {
		return time.Duration(n) * time.Second
	}
	return def
}
