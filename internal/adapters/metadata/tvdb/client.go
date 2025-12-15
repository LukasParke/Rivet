package tvdb

import (
	"bytes"
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
		baseURL = "https://api4.thetvdb.com"
	}
	u, err := security.ValidateOutboundURL(baseURL, policy)
	if err != nil {
		return nil, err
	}
	return &Client{baseURL: u, apiKey: apiKey, policy: policy, http: &http.Client{Timeout: 8 * time.Second}}, nil
}

func (c *Client) Validate(ctx context.Context) (string, error) {
	// TVDB v4 requires a token obtained from /v4/login.
	loginURL := c.baseURL.ResolveReference(&url.URL{Path: "/v4/login"})
	payload := map[string]string{"apikey": c.apiKey}
	b, _ := json.Marshal(payload)

	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, loginURL.String(), bytes.NewReader(b))
	req.Header.Set("content-type", "application/json")
	res, err := c.http.Do(req)
	if err != nil {
		return "", errors.New("connect_failed")
	}
	defer res.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(res.Body, 1<<20))
	if res.StatusCode == 401 || res.StatusCode == 403 {
		return "", errors.New("invalid_api_key")
	}
	if res.StatusCode != 200 {
		return "", errors.New("http_error")
	}

	var out struct {
		Data struct {
			Token string `json:"token"`
		} `json:"data"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		return "", errors.New("parse_failed")
	}
	if out.Data.Token == "" {
		return "", errors.New("unexpected_response")
	}
	return "token_ok", nil
}
