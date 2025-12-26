package qbittorrent

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/url"
	"strings"
)

type TorrentInfo struct {
	Hash       string  `json:"hash"`
	Name       string  `json:"name"`
	Progress   float64 `json:"progress"`
	State      string  `json:"state"`
	ContentPath string `json:"content_path"`
	SavePath   string  `json:"save_path"`
	CompletionOn int64 `json:"completion_on"`
}

// GetAllTorrents returns all torrents from qBittorrent.
func (c *Client) GetAllTorrents(ctx context.Context) ([]TorrentInfo, error) {
	cookie, err := c.login(ctx)
	if err != nil {
		return nil, err
	}

	infoURL := c.baseURL.ResolveReference(&url.URL{Path: "/api/v2/torrents/info"})

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, infoURL.String(), nil)
	if cookie != "" {
		req.Header.Set("cookie", cookie)
	}
	res, err := c.http.Do(req)
	if err != nil {
		return nil, errors.New("connect_failed")
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		return nil, errors.New("info_failed")
	}
	b, _ := io.ReadAll(io.LimitReader(res.Body, 1<<20))
	var arr []TorrentInfo
	if err := json.Unmarshal(b, &arr); err != nil {
		return nil, errors.New("parse_failed")
	}
	return arr, nil
}

func (c *Client) GetInfo(ctx context.Context, hash string) (TorrentInfo, bool, error) {
	hash = strings.TrimSpace(hash)
	if hash == "" {
		return TorrentInfo{}, false, errors.New("hash_required")
	}

	cookie, err := c.login(ctx)
	if err != nil {
		return TorrentInfo{}, false, err
	}

	infoURL := c.baseURL.ResolveReference(&url.URL{Path: "/api/v2/torrents/info"})
	q := infoURL.Query()
	q.Set("hashes", hash)
	infoURL.RawQuery = q.Encode()

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, infoURL.String(), nil)
	if cookie != "" {
		req.Header.Set("cookie", cookie)
	}
	res, err := c.http.Do(req)
	if err != nil {
		return TorrentInfo{}, false, errors.New("connect_failed")
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		return TorrentInfo{}, false, errors.New("info_failed")
	}
	b, _ := io.ReadAll(io.LimitReader(res.Body, 1<<20))
	var arr []TorrentInfo
	if err := json.Unmarshal(b, &arr); err != nil {
		return TorrentInfo{}, false, errors.New("parse_failed")
	}
	if len(arr) == 0 {
		return TorrentInfo{}, false, nil
	}
	return arr[0], true, nil
}

func IsCompletedState(state string) bool {
	s := strings.ToLower(strings.TrimSpace(state))
	// qB states vary; treat these as complete enough.
	switch s {
	case "uploading", "stalledup", "pausedup", "checkingup", "forcedup":
		return true
	default:
		return false
	}
}

func IsPausedState(state string) bool {
	s := strings.ToLower(strings.TrimSpace(state))
	switch s {
	case "pauseddl", "pausedup":
		return true
	default:
		return false
	}
}
