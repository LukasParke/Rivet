package dryrun

import (
	"context"
	"crypto/sha1"
	"encoding/hex"
	"errors"
	"net/url"
	"strings"
	"sync"
	"time"

	"github.com/LukeHagar/Rivet/internal/adapters/downloaders/qbittorrent"
)

// Client simulates a qBittorrent-like downloader for local testing.
// It keeps state in-memory and advances progress deterministically over time.
//
// Intended use: DRY_RUN=1 (simulate download client + filesystem writes).
type Client struct {
	mu sync.Mutex

	fixturePath string
	duration    time.Duration

	// downloads keyed by "hash" (btih-like external ID).
	downloads map[string]*download
}

type download struct {
	hash string
	name string

	startedAt time.Time
	paused    bool
	elapsed   time.Duration

	// When true, the download is removed (delete).
	deleted bool
}

type Config struct {
	// FixturePath optionally points to a local file/dir used as the content path for
	// verification/import in dry run. This is read-only in dry run.
	FixturePath string

	// Duration controls how long a simulated download takes from 0->100%.
	// If 0, a default is used.
	Duration time.Duration
}

func New(cfg Config) *Client {
	if cfg.Duration <= 0 {
		cfg.Duration = 25 * time.Second
	}
	return &Client{
		fixturePath: strings.TrimSpace(cfg.FixturePath),
		duration:    cfg.Duration,
		downloads:   map[string]*download{},
	}
}

var (
	defaultMu     sync.Mutex
	defaultClient *Client
)

// Default returns a singleton dry-run client so state is shared across handlers/routes
// within the same process.
func Default(fixturePath string) *Client {
	defaultMu.Lock()
	defer defaultMu.Unlock()

	if defaultClient == nil {
		defaultClient = New(Config{FixturePath: fixturePath})
		return defaultClient
	}
	// If a fixture path becomes available later, adopt it.
	fp := strings.TrimSpace(fixturePath)
	if fp != "" && defaultClient.fixturePath == "" {
		defaultClient.fixturePath = fp
	}
	return defaultClient
}

func (c *Client) AddURL(ctx context.Context, torrentURL string) error {
	_ = ctx
	torrentURL = strings.TrimSpace(torrentURL)
	if torrentURL == "" {
		return errors.New("torrent_url_required")
	}

	hash := extractBTIH(torrentURL)
	if hash == "" {
		sum := sha1.Sum([]byte(torrentURL))
		hash = hex.EncodeToString(sum[:])
	}
	name := deriveName(torrentURL)

	c.mu.Lock()
	defer c.mu.Unlock()
	if d, ok := c.downloads[hash]; ok && !d.deleted {
		// Already added; treat as success.
		return nil
	}
	c.downloads[hash] = &download{
		hash:      hash,
		name:      name,
		startedAt: time.Now().UTC(),
	}
	return nil
}

// GetAllTorrents returns all active downloads in dry run mode, sorted by startedAt (most recent first).
func (c *Client) GetAllTorrents(ctx context.Context) ([]qbittorrent.TorrentInfo, error) {
	_ = ctx
	c.mu.Lock()
	defer c.mu.Unlock()

	type torrentWithTime struct {
		info      qbittorrent.TorrentInfo
		startedAt time.Time
	}

	var items []torrentWithTime
	now := time.Now().UTC()

	for _, d := range c.downloads {
		if d.deleted {
			continue
		}

		elapsed := d.elapsed
		if !d.paused {
			elapsed += now.Sub(d.startedAt)
		}
		progress := float64(elapsed) / float64(c.duration)
		if progress < 0 {
			progress = 0
		}
		if progress > 1 {
			progress = 1
		}
		state := "downloading"
		if d.paused {
			state = "pauseddl"
		} else if progress >= 0.999 {
			state = "uploading"
		}

		contentPath := "dryrun://" + d.hash
		if c.fixturePath != "" {
			contentPath = c.fixturePath
		}
		items = append(items, torrentWithTime{
			info: qbittorrent.TorrentInfo{
				Hash:        d.hash,
				Name:        d.name,
				Progress:    progress,
				State:       state,
				ContentPath: contentPath,
				SavePath:    "/dryrun",
			},
			startedAt: d.startedAt,
		})
	}

	// Sort by startedAt descending (most recent first)
	for i := 0; i < len(items); i++ {
		for j := i + 1; j < len(items); j++ {
			if items[i].startedAt.Before(items[j].startedAt) {
				items[i], items[j] = items[j], items[i]
			}
		}
	}

	result := make([]qbittorrent.TorrentInfo, len(items))
	for i, item := range items {
		result[i] = item.info
	}

	return result, nil
}

func (c *Client) GetInfo(ctx context.Context, hash string) (qbittorrent.TorrentInfo, bool, error) {
	_ = ctx
	hash = strings.TrimSpace(hash)
	if hash == "" {
		return qbittorrent.TorrentInfo{}, false, errors.New("hash_required")
	}

	c.mu.Lock()
	d, ok := c.downloads[hash]
	if !ok || d.deleted {
		c.mu.Unlock()
		return qbittorrent.TorrentInfo{}, false, nil
	}

	now := time.Now().UTC()
	elapsed := d.elapsed
	if !d.paused {
		elapsed += now.Sub(d.startedAt)
	}
	progress := float64(elapsed) / float64(c.duration)
	if progress < 0 {
		progress = 0
	}
	if progress > 1 {
		progress = 1
	}
	state := "downloading"
	if d.paused {
		state = "pauseddl"
	} else if progress >= 0.999 {
		state = "uploading"
	}

	contentPath := "dryrun://" + d.hash
	if c.fixturePath != "" {
		contentPath = c.fixturePath
	}
	info := qbittorrent.TorrentInfo{
		Hash:        d.hash,
		Name:        d.name,
		Progress:    progress,
		State:       state,
		ContentPath: contentPath,
		SavePath:    "/dryrun",
	}
	c.mu.Unlock()
	return info, true, nil
}

func (c *Client) Resume(ctx context.Context, hash string) error {
	_ = ctx
	hash = strings.TrimSpace(hash)
	if hash == "" {
		return errors.New("hash_required")
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	d, ok := c.downloads[hash]
	if !ok || d.deleted {
		return nil
	}
	if !d.paused {
		return nil
	}
	d.paused = false
	d.startedAt = time.Now().UTC()
	return nil
}

func (c *Client) Pause(ctx context.Context, hash string) error {
	_ = ctx
	hash = strings.TrimSpace(hash)
	if hash == "" {
		return errors.New("hash_required")
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	d, ok := c.downloads[hash]
	if !ok || d.deleted {
		return nil
	}
	if d.paused {
		return nil
	}
	d.elapsed += time.Since(d.startedAt)
	d.paused = true
	return nil
}

func (c *Client) Recheck(ctx context.Context, hash string) error {
	_ = ctx
	_ = hash
	// No-op in dry run.
	return nil
}

func (c *Client) Reannounce(ctx context.Context, hash string) error {
	_ = ctx
	_ = hash
	// No-op in dry run.
	return nil
}

func (c *Client) Delete(ctx context.Context, hash string, deleteFiles bool) error {
	_ = ctx
	_ = deleteFiles
	hash = strings.TrimSpace(hash)
	if hash == "" {
		return errors.New("hash_required")
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if d, ok := c.downloads[hash]; ok {
		d.deleted = true
	}
	return nil
}

func extractBTIH(u string) string {
	u = strings.TrimSpace(u)
	if !strings.HasPrefix(strings.ToLower(u), "magnet:?") {
		return ""
	}
	lower := strings.ToLower(u)
	idx := strings.Index(lower, "xt=urn:btih:")
	if idx == -1 {
		return ""
	}
	start := idx + len("xt=urn:btih:")
	end := start
	for end < len(u) {
		ch := u[end]
		if ch == '&' || ch == '#' {
			break
		}
		end++
	}
	return strings.TrimSpace(u[start:end])
}

func deriveName(u string) string {
	u = strings.TrimSpace(u)
	if strings.HasPrefix(strings.ToLower(u), "magnet:?") {
		if pu, err := url.Parse(u); err == nil {
			if dn := strings.TrimSpace(pu.Query().Get("dn")); dn != "" {
				return dn
			}
		}
	}
	// fallback
	return "dryrun-download"
}


