package handlers

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"strings"
	"time"

	"github.com/LukeHagar/Rivet/internal/adapters/downloaders/qbittorrent"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type AcquisitionDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy

	// For tests: override client factory.
	NewQb func(cfg qbittorrent.Config, policy security.OutboundPolicy) (qbAdder, error)
}

type qbAdder interface {
	AddURL(ctx context.Context, torrentURL string) error
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
		cfg, err := loadQBittorrentConfig(ctx, deps)
		if err != nil {
			return err
		}

		factory := deps.NewQb
		if factory == nil {
			factory = func(cfg qbittorrent.Config, policy security.OutboundPolicy) (qbAdder, error) {
				return qbittorrent.New(cfg, policy)
			}
		}
		cli, err := factory(cfg, deps.Policy)
		if err != nil {
			return err
		}
		if err := cli.AddURL(ctx, urlToUse); err != nil {
			_ = storage.MarkAcquisitionFailed(ctx, deps.DB, a.ID, err.Error())
			return err
		}
		_ = storage.MarkAcquisitionEnqueued(ctx, deps.DB, a.ID, nil)
		return nil
	default:
		return errors.New("unsupported_client_kind")
	}
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
