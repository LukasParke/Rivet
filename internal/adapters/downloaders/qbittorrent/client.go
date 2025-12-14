package qbittorrent

import (
	"context"
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
	user    string
	pass    string
	http    *http.Client
	policy  security.OutboundPolicy
}

type Config struct {
	BaseURL  string
	Username string
	Password string
}

func New(cfg Config, policy security.OutboundPolicy) (*Client, error) {
	cfg.BaseURL = strings.TrimSpace(cfg.BaseURL)
	cfg.Username = strings.TrimSpace(cfg.Username)
	cfg.Password = strings.TrimSpace(cfg.Password)
	if cfg.BaseURL == "" || cfg.Username == "" || cfg.Password == "" {
		return nil, errors.New("missing_fields")
	}
	u, err := security.ValidateOutboundURL(cfg.BaseURL, policy)
	if err != nil {
		return nil, err
	}
	return &Client{baseURL: u, user: cfg.Username, pass: cfg.Password, http: &http.Client{Timeout: 10 * time.Second}, policy: policy}, nil
}

func (c *Client) AddURL(ctx context.Context, torrentURL string) error {
	torrentURL = strings.TrimSpace(torrentURL)
	if torrentURL == "" {
		return errors.New("torrent_url_required")
	}

	cookie, err := c.login(ctx)
	if err != nil {
		return err
	}

	addURL := c.baseURL.ResolveReference(&url.URL{Path: "/api/v2/torrents/add"})
	form := url.Values{}
	form.Set("urls", torrentURL)

	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, addURL.String(), strings.NewReader(form.Encode()))
	req.Header.Set("content-type", "application/x-www-form-urlencoded")
	if cookie != "" {
		req.Header.Set("cookie", cookie)
	}

	res, err := c.http.Do(req)
	if err != nil {
		return errors.New("connect_failed")
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(res.Body, 2048))
	if res.StatusCode != 200 {
		return errors.New("add_failed")
	}
	if strings.TrimSpace(string(b)) != "Ok." {
		// Some instances return Ok.
		return errors.New("add_failed")
	}
	return nil
}

func (c *Client) login(ctx context.Context) (string, error) {
	loginURL := c.baseURL.ResolveReference(&url.URL{Path: "/api/v2/auth/login"})
	form := url.Values{}
	form.Set("username", c.user)
	form.Set("password", c.pass)

	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, loginURL.String(), strings.NewReader(form.Encode()))
	req.Header.Set("content-type", "application/x-www-form-urlencoded")
	res, err := c.http.Do(req)
	if err != nil {
		return "", errors.New("connect_failed")
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(res.Body, 1024))
	if res.StatusCode != 200 {
		return "", errors.New("login_failed")
	}
	if strings.TrimSpace(string(b)) != "Ok." {
		return "", errors.New("login_failed")
	}
	return res.Header.Get("set-cookie"), nil
}
