package tmdb

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/LukeHagar/Rivet/internal/security"
)

type Client struct {
	baseURL *url.URL
	apiKey  string
	policy  security.OutboundPolicy
	http    *http.Client
}

// Fallback TMDB API key for testing/development (rate-limited, shared across users).
// Users should get their own free API key at https://www.themoviedb.org/settings/api
// for better rate limits and ToS compliance.
const fallbackTMDBAPIKey = "c9856d0cb57c3f14bf75bdc6c063b8f3"

func New(baseURL, apiKey string, policy security.OutboundPolicy) (*Client, error) {
	apiKey = strings.TrimSpace(apiKey)
	// Allow empty API key and use fallback for out-of-box experience.
	// This is rate-limited and users should get their own key for production use.
	if apiKey == "" {
		apiKey = fallbackTMDBAPIKey
	}
	if baseURL == "" {
		baseURL = "https://api.themoviedb.org"
	}
	u, err := security.ValidateOutboundURL(baseURL, policy)
	if err != nil {
		return nil, err
	}
	return &Client{baseURL: u, apiKey: apiKey, policy: policy, http: &http.Client{Timeout: 8 * time.Second}}, nil
}

func (c *Client) Validate(ctx context.Context) (string, error) {
	// Hit /3/configuration as a cheap auth check.
	u := c.baseURL.ResolveReference(&url.URL{Path: "/3/configuration"})
	q := u.Query()
	q.Set("api_key", c.apiKey)
	u.RawQuery = q.Encode()

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, u.String(), nil)
	res, err := c.http.Do(req)
	if err != nil {
		return "", errors.New("connect_failed")
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(res.Body, 1<<20))
	if res.StatusCode == 401 || res.StatusCode == 403 {
		return "", errors.New("invalid_api_key")
	}
	if res.StatusCode != 200 {
		return "", errors.New("http_error")
	}

	var out struct {
		Images struct {
			BaseURL string `json:"base_url"`
		} `json:"images"`
	}
	if err := json.Unmarshal(b, &out); err != nil {
		return "", errors.New("parse_failed")
	}
	if out.Images.BaseURL == "" {
		return "", errors.New("unexpected_response")
	}
	return out.Images.BaseURL, nil
}
