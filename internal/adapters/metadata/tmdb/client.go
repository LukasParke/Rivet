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

func New(baseURL, apiKey string, policy security.OutboundPolicy) (*Client, error) {
	apiKey = strings.TrimSpace(apiKey)
	if apiKey == "" {
		return nil, errors.New("api key required")
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
