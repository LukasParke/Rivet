package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"strings"
	"time"

	"github.com/LukeHagar/Rivet/internal/adapters/downloaders/dryrun"
	"github.com/LukeHagar/Rivet/internal/adapters/downloaders/qbittorrent"
	"github.com/LukeHagar/Rivet/internal/jobs/queue"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type AcquisitionDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
	Hub       publisher

	DryRun            bool
	DryRunFixturePath string

	// For tests: override client factory.
	NewQb func(cfg qbittorrent.Config, policy security.OutboundPolicy) (qbClient, error)
}

type publisher interface {
	Publish(eventType string, payload any)
}

type qbClient interface {
	AddURL(ctx context.Context, torrentURL string) error
	GetInfo(ctx context.Context, hash string) (qbittorrent.TorrentInfo, bool, error)
	GetAllTorrents(ctx context.Context) ([]qbittorrent.TorrentInfo, error)
	Resume(ctx context.Context, hash string) error
	Pause(ctx context.Context, hash string) error
	Recheck(ctx context.Context, hash string) error
	Reannounce(ctx context.Context, hash string) error
	Delete(ctx context.Context, hash string, deleteFiles bool) error
}

type persistedClients struct {
	QBittorrent *struct {
		URL         string `json:"url"`
		Username    string `json:"username"`
		PasswordEnc string `json:"passwordEnc"`
	} `json:"qbittorrent"`
}

func EnqueueAcquisition(ctx context.Context, deps AcquisitionDeps, acquisitionID string) error {
	ctx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()

	a, err := storage.GetAcquisition(ctx, deps.DB, acquisitionID)
	if err != nil {
		return err
	}

	// Load candidate raw JSON for enclosure URL.
	var rawJSON string
	row := deps.DB.QueryRowContext(ctx, `SELECT raw_json FROM release_candidates WHERE id = ?`, a.CandidateID)
	if err := row.Scan(&rawJSON); err != nil {
		return err
	}
	var raw struct {
		EnclosureURL string `json:"enclosureUrl"`
		Link         string `json:"link"`
		Title        string `json:"title"`
	}
	_ = json.Unmarshal([]byte(rawJSON), &raw)
	urlToUse := strings.TrimSpace(raw.EnclosureURL)
	if urlToUse == "" {
		urlToUse = strings.TrimSpace(raw.Link)
	}
	if urlToUse == "" {
		return errors.New("candidate_missing_url")
	}

	switch a.ClientKind {
	case "qbittorrent":
		var cli qbClient
		if deps.DryRun {
			cli = dryrun.Default(deps.DryRunFixturePath)
		} else {
			cfg, err := loadQBittorrentConfig(ctx, deps)
			if err != nil {
				return err
			}
			factory := deps.NewQb
			if factory == nil {
				factory = func(cfg qbittorrent.Config, policy security.OutboundPolicy) (qbClient, error) {
					return qbittorrent.New(cfg, policy)
				}
			}
			c, err := factory(cfg, deps.Policy)
			if err != nil {
				return err
			}
			cli = c
		}

		// Store source URL for diagnostics.
		_ = setAcquisitionSourceURL(ctx, deps.DB, a.ID, urlToUse)

		// Prefer extracting hash from magnet to track by hash later.
		hash := extractBTIH(urlToUse)

		if err := cli.AddURL(ctx, urlToUse); err != nil {
			_ = storage.MarkAcquisitionFailed(ctx, deps.DB, a.ID, err.Error())
			if deps.Hub != nil {
				deps.Hub.Publish("download.failed", map[string]any{
					"acquisitionId": a.ID,
					"candidateId":   a.CandidateID,
					"targetId":      a.TargetID,
					"clientKind":    a.ClientKind,
					"clientName":    a.ClientName,
					"error":         err.Error(),
				})
			}
			// Rescout v1: attempt to find an alternative when enqueue fails.
			_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_rescout", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(1 * time.Second)})
			return err
		}

		// If we don't have a hash from the URL (e.g., Prowlarr redirect URLs), try to find it.
		if hash == "" {
			if deps.DryRun {
				// In dry run, the client generates a hash from the URL. Query it to get the hash.
				// Wait a moment for the dry run client to process the add
				time.Sleep(100 * time.Millisecond)
				allTorrents, err := cli.GetAllTorrents(ctx)
				if err == nil && len(allTorrents) > 0 {
					// Try to match by candidate title (fuzzy match)
					candidateTitle := strings.ToLower(strings.TrimSpace(raw.Title))
					for _, t := range allTorrents {
						torrentName := strings.ToLower(strings.TrimSpace(t.Name))
						// Simple matching: if the torrent name contains significant parts of the candidate title
						if strings.Contains(torrentName, candidateTitle) || strings.Contains(candidateTitle, torrentName) {
							hash = t.Hash
							break
						}
					}
					// If no match found, use the most recently added torrent (first in list, sorted by startedAt)
					if hash == "" && len(allTorrents) > 0 {
						// In dry run, GetAllTorrents returns torrents sorted by startedAt (most recent first)
						hash = allTorrents[0].Hash
					}
				}
			} else {
				// Real qBittorrent: wait a moment for qBittorrent to process the add
				time.Sleep(500 * time.Millisecond)
				// Query all torrents and find the one matching our candidate title
				allTorrents, err := cli.GetAllTorrents(ctx)
				if err == nil && len(allTorrents) > 0 {
					// Try to match by candidate title (fuzzy match)
					candidateTitle := strings.ToLower(strings.TrimSpace(raw.Title))
					for _, t := range allTorrents {
						torrentName := strings.ToLower(strings.TrimSpace(t.Name))
						// Simple matching: if the torrent name contains significant parts of the candidate title
						if strings.Contains(torrentName, candidateTitle) || strings.Contains(candidateTitle, torrentName) {
							hash = t.Hash
							break
						}
					}
					// If no match found, use the most recently added torrent (assuming it's the one we just added)
					if hash == "" && len(allTorrents) > 0 {
						// Try the first torrent with low progress (likely just added)
						for _, t := range allTorrents {
							if t.Progress < 0.1 && t.State != "pausedup" && t.State != "uploading" {
								hash = t.Hash
								break
							}
						}
						// Last resort: use the first torrent
						if hash == "" {
							hash = allTorrents[0].Hash
						}
					}
				}
			}
		}

		var extID *string
		if hash != "" {
			extID = &hash
		}
		_ = storage.MarkAcquisitionEnqueued(ctx, deps.DB, a.ID, extID)
		if deps.Hub != nil {
			deps.Hub.Publish("download.enqueued", map[string]any{
				"acquisitionId": a.ID,
				"candidateId":   a.CandidateID,
				"targetId":      a.TargetID,
				"clientKind":    a.ClientKind,
				"clientName":    a.ClientName,
				"sourceUrl":     urlToUse,
				"hash":          hash,
			})
		}
		_, _ = queue.Enqueue(ctx, deps.DB, "acquisition_monitor", map[string]string{"acquisitionId": a.ID}, queue.EnqueueOpts{RunAfter: time.Now().UTC().Add(2 * time.Second)})
		return nil
	default:
		return errors.New("unsupported_client_kind")
	}
}

func setAcquisitionSourceURL(ctx context.Context, db *sql.DB, id string, urlStr string) error {
	now := time.Now().UTC()
	_, err := db.ExecContext(ctx, `UPDATE acquisitions SET source_url = ?, updated_at = ? WHERE id = ?`, urlStr, now.Format(time.RFC3339Nano), id)
	return err
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
	h := u[start:end]
	return strings.TrimSpace(h)
}

func loadQBittorrentConfig(ctx context.Context, deps AcquisitionDeps) (qbittorrent.Config, error) {
	cfgStr, ok, err := storage.GetSetting(ctx, deps.DB, "setup.clients")
	if err != nil {
		return qbittorrent.Config{}, err
	}
	if !ok {
		return qbittorrent.Config{}, errors.New("no_clients_configured")
	}
	var p persistedClients
	if err := json.Unmarshal([]byte(cfgStr), &p); err != nil {
		return qbittorrent.Config{}, err
	}
	if p.QBittorrent == nil {
		return qbittorrent.Config{}, errors.New("qbittorrent_not_configured")
	}
	pw, err := deps.SecretBox.DecryptString(p.QBittorrent.PasswordEnc)
	if err != nil {
		return qbittorrent.Config{}, errors.New("decrypt_failed")
	}
	return qbittorrent.Config{BaseURL: p.QBittorrent.URL, Username: p.QBittorrent.Username, Password: string(pw)}, nil
}
