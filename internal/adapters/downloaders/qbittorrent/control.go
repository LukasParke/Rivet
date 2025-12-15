package qbittorrent

import (
	"context"
	"errors"
	"io"
	"net/http"
	"net/url"
	"strings"
)

func (c *Client) Resume(ctx context.Context, hash string) error {
	return c.postHashes(ctx, "/api/v2/torrents/resume", hash)
}

func (c *Client) Pause(ctx context.Context, hash string) error {
	return c.postHashes(ctx, "/api/v2/torrents/pause", hash)
}

func (c *Client) Recheck(ctx context.Context, hash string) error {
	return c.postHashes(ctx, "/api/v2/torrents/recheck", hash)
}

func (c *Client) Reannounce(ctx context.Context, hash string) error {
	return c.postHashes(ctx, "/api/v2/torrents/reannounce", hash)
}

func (c *Client) Delete(ctx context.Context, hash string, deleteFiles bool) error {
	hash = strings.TrimSpace(hash)
	if hash == "" {
		return errors.New("hash_required")
	}
	cookie, err := c.login(ctx)
	if err != nil {
		return err
	}
	u := c.baseURL.ResolveReference(&url.URL{Path: "/api/v2/torrents/delete"})
	form := url.Values{}
	form.Set("hashes", hash)
	if deleteFiles {
		form.Set("deleteFiles", "true")
	} else {
		form.Set("deleteFiles", "false")
	}
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, u.String(), strings.NewReader(form.Encode()))
	req.Header.Set("content-type", "application/x-www-form-urlencoded")
	if cookie != "" {
		req.Header.Set("cookie", cookie)
	}
	res, err := c.http.Do(req)
	if err != nil {
		return errors.New("connect_failed")
	}
	defer res.Body.Close()
	_, _ = io.ReadAll(io.LimitReader(res.Body, 1024))
	if res.StatusCode != 200 {
		return errors.New("control_failed")
	}
	return nil
}

func (c *Client) postHashes(ctx context.Context, path string, hash string) error {
	hash = strings.TrimSpace(hash)
	if hash == "" {
		return errors.New("hash_required")
	}
	cookie, err := c.login(ctx)
	if err != nil {
		return err
	}
	u := c.baseURL.ResolveReference(&url.URL{Path: path})
	form := url.Values{}
	form.Set("hashes", hash)
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, u.String(), strings.NewReader(form.Encode()))
	req.Header.Set("content-type", "application/x-www-form-urlencoded")
	if cookie != "" {
		req.Header.Set("cookie", cookie)
	}
	res, err := c.http.Do(req)
	if err != nil {
		return errors.New("connect_failed")
	}
	defer res.Body.Close()
	_, _ = io.ReadAll(io.LimitReader(res.Body, 1024))
	if res.StatusCode != 200 {
		return errors.New("control_failed")
	}
	return nil
}
