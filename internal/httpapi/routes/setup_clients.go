package routes

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type SetupClientsDeps struct {
	DB *sql.DB

	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
}

type QBittorrentConfig struct {
	URL      string `json:"url"`
	Username string `json:"username"`
	Password string `json:"password"`
}

type NZBGetConfig struct {
	URL      string `json:"url"`
	Username string `json:"username"`
	Password string `json:"password"`
}

type SABnzbdConfig struct {
	URL    string `json:"url"`
	APIKey string `json:"apiKey"`
}

type SetupClientsValidateRequest struct {
	QBittorrent *QBittorrentConfig `json:"qbittorrent,omitempty"`
	NZBGet      *NZBGetConfig      `json:"nzbget,omitempty"`
	SABnzbd     *SABnzbdConfig     `json:"sabnzbd,omitempty"`
}

type ClientTestResult struct {
	Ok      bool              `json:"ok"`
	Details map[string]string `json:"details,omitempty"`
	Error   string            `json:"error,omitempty"`
}

type SetupClientsValidateResponse struct {
	Ok bool `json:"ok"`

	QBittorrent *ClientTestResult `json:"qbittorrent,omitempty"`
	NZBGet      *ClientTestResult `json:"nzbget,omitempty"`
	SABnzbd     *ClientTestResult `json:"sabnzbd,omitempty"`
}

type SetupClientsSaveResponse struct {
	Saved bool `json:"saved"`
}

func RegisterSetupClients(api fiber.Router, deps SetupClientsDeps) {
	grp := api.Group("/setup")

	grp.Post("/clients/validate", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupClientsValidateRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}
		ctx, cancel := context.WithTimeout(c.UserContext(), 12*time.Second)
		defer cancel()

		resp := validateClients(ctx, deps, req)
		return c.JSON(resp)
	})

	grp.Post("/clients/save", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupClientsValidateRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}
		ctx, cancel := context.WithTimeout(c.UserContext(), 12*time.Second)
		defer cancel()

		resp := validateClients(ctx, deps, req)
		if !resp.Ok {
			return c.Status(http.StatusBadRequest).JSON(resp)
		}

		// Encrypt secrets before persisting.
		persist := map[string]any{}
		if req.QBittorrent != nil {
			pwEnc, _ := deps.SecretBox.EncryptToString([]byte(req.QBittorrent.Password))
			persist["qbittorrent"] = map[string]any{
				"url":      strings.TrimSpace(req.QBittorrent.URL),
				"username": strings.TrimSpace(req.QBittorrent.Username),
				"passwordEnc": pwEnc,
			}
		}
		if req.NZBGet != nil {
			pwEnc, _ := deps.SecretBox.EncryptToString([]byte(req.NZBGet.Password))
			persist["nzbget"] = map[string]any{
				"url":      strings.TrimSpace(req.NZBGet.URL),
				"username": strings.TrimSpace(req.NZBGet.Username),
				"passwordEnc": pwEnc,
			}
		}
		if req.SABnzbd != nil {
			keyEnc, _ := deps.SecretBox.EncryptToString([]byte(req.SABnzbd.APIKey))
			persist["sabnzbd"] = map[string]any{
				"url":       strings.TrimSpace(req.SABnzbd.URL),
				"apiKeyEnc": keyEnc,
			}
		}

		b, _ := json.Marshal(persist)
		if err := storage.UpsertSetting(ctx, deps.DB, "setup.clients", string(b)); err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "save_failed"})
		}
		_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{Action: "setup.clients_saved"})

		return c.JSON(SetupClientsSaveResponse{Saved: true})
	})
}

func validateClients(ctx context.Context, deps SetupClientsDeps, req SetupClientsValidateRequest) SetupClientsValidateResponse {
	resp := SetupClientsValidateResponse{Ok: true}

	// Require at least one configured client.
	if req.QBittorrent == nil && req.NZBGet == nil && req.SABnzbd == nil {
		return SetupClientsValidateResponse{Ok: false}
	}

	client := &http.Client{Timeout: 6 * time.Second}

	if req.QBittorrent != nil {
		r := testQBittorrent(ctx, client, deps.Policy, *req.QBittorrent)
		resp.QBittorrent = &r
		resp.Ok = resp.Ok && r.Ok
	}
	if req.NZBGet != nil {
		r := testNZBGet(ctx, client, deps.Policy, *req.NZBGet)
		resp.NZBGet = &r
		resp.Ok = resp.Ok && r.Ok
	}
	if req.SABnzbd != nil {
		r := testSABnzbd(ctx, client, deps.Policy, *req.SABnzbd)
		resp.SABnzbd = &r
		resp.Ok = resp.Ok && r.Ok
	}

	return resp
}

func testQBittorrent(ctx context.Context, client *http.Client, p security.OutboundPolicy, cfg QBittorrentConfig) ClientTestResult {
	cfg.URL = strings.TrimSpace(cfg.URL)
	cfg.Username = strings.TrimSpace(cfg.Username)
	cfg.Password = strings.TrimSpace(cfg.Password)
	if cfg.URL == "" || cfg.Username == "" || cfg.Password == "" {
		return ClientTestResult{Ok: false, Error: "missing_fields"}
	}

	base, err := security.ValidateOutboundURL(cfg.URL, p)
	if err != nil {
		return ClientTestResult{Ok: false, Error: err.Error()}
	}

	// Login (sets cookie)
	loginURL := base.ResolveReference(&url.URL{Path: "/api/v2/auth/login"})
	form := url.Values{}
	form.Set("username", cfg.Username)
	form.Set("password", cfg.Password)

	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, loginURL.String(), strings.NewReader(form.Encode()))
	req.Header.Set("content-type", "application/x-www-form-urlencoded")
	res, err := client.Do(req)
	if err != nil {
		return ClientTestResult{Ok: false, Error: "connect_failed"}
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(res.Body, 1024))
	if res.StatusCode != 200 {
		return ClientTestResult{Ok: false, Error: "login_failed"}
	}
	if strings.TrimSpace(string(b)) != "Ok." {
		// qBittorrent returns Ok. on success.
		return ClientTestResult{Ok: false, Error: "login_failed"}
	}

	cookie := res.Header.Get("set-cookie")
	if cookie == "" {
		// still try version call; some proxies strip headers
	}

	verURL := base.ResolveReference(&url.URL{Path: "/api/v2/app/version"})
	vreq, _ := http.NewRequestWithContext(ctx, http.MethodGet, verURL.String(), nil)
	if cookie != "" {
		vreq.Header.Set("cookie", cookie)
	}
	vres, err := client.Do(vreq)
	if err != nil {
		return ClientTestResult{Ok: false, Error: "version_failed"}
	}
	defer vres.Body.Close()
	vb, _ := io.ReadAll(io.LimitReader(vres.Body, 1024))
	if vres.StatusCode != 200 {
		return ClientTestResult{Ok: false, Error: "version_failed"}
	}

	return ClientTestResult{Ok: true, Details: map[string]string{"version": strings.TrimSpace(string(vb))}}
}

func testNZBGet(ctx context.Context, client *http.Client, p security.OutboundPolicy, cfg NZBGetConfig) ClientTestResult {
	cfg.URL = strings.TrimSpace(cfg.URL)
	cfg.Username = strings.TrimSpace(cfg.Username)
	cfg.Password = strings.TrimSpace(cfg.Password)
	if cfg.URL == "" || cfg.Username == "" || cfg.Password == "" {
		return ClientTestResult{Ok: false, Error: "missing_fields"}
	}

	base, err := security.ValidateOutboundURL(cfg.URL, p)
	if err != nil {
		return ClientTestResult{Ok: false, Error: err.Error()}
	}

	// NZBGet JSON-RPC: POST /jsonrpc
	rpcURL := base.ResolveReference(&url.URL{Path: "/jsonrpc"})
	payload := map[string]any{
		"method": "version",
		"params": []any{},
		"id":     1,
	}
	b, _ := json.Marshal(payload)
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, rpcURL.String(), bytes.NewReader(b))
	req.Header.Set("content-type", "application/json")
	req.SetBasicAuth(cfg.Username, cfg.Password)
	res, err := client.Do(req)
	if err != nil {
		return ClientTestResult{Ok: false, Error: "connect_failed"}
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		return ClientTestResult{Ok: false, Error: "auth_failed"}
	}

	respB, _ := io.ReadAll(io.LimitReader(res.Body, 4096))
	var out struct {
		Result any `json:"result"`
		Error  any `json:"error"`
	}
	_ = json.Unmarshal(respB, &out)
	if out.Error != nil {
		return ClientTestResult{Ok: false, Error: "rpc_error"}
	}
	ver := ""
	switch v := out.Result.(type) {
	case string:
		ver = v
	}
	if ver == "" {
		// Some installs use method "status"; fall back.
		ver = "ok"
	}
	return ClientTestResult{Ok: true, Details: map[string]string{"version": ver}}
}

func testSABnzbd(ctx context.Context, client *http.Client, p security.OutboundPolicy, cfg SABnzbdConfig) ClientTestResult {
	cfg.URL = strings.TrimSpace(cfg.URL)
	cfg.APIKey = strings.TrimSpace(cfg.APIKey)
	if cfg.URL == "" || cfg.APIKey == "" {
		return ClientTestResult{Ok: false, Error: "missing_fields"}
	}

	base, err := security.ValidateOutboundURL(cfg.URL, p)
	if err != nil {
		return ClientTestResult{Ok: false, Error: err.Error()}
	}

	// SABnzbd API: GET /api?mode=version&output=json&apikey=...
	apiURL := base.ResolveReference(&url.URL{Path: "/api"})
	q := apiURL.Query()
	q.Set("mode", "version")
	q.Set("output", "json")
	q.Set("apikey", cfg.APIKey)
	apiURL.RawQuery = q.Encode()

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, apiURL.String(), nil)
	res, err := client.Do(req)
	if err != nil {
		return ClientTestResult{Ok: false, Error: "connect_failed"}
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		return ClientTestResult{Ok: false, Error: "api_failed"}
	}

	respB, _ := io.ReadAll(io.LimitReader(res.Body, 4096))
	var out struct {
		Version string `json:"version"`
	}
	if err := json.Unmarshal(respB, &out); err != nil {
		return ClientTestResult{Ok: false, Error: "parse_failed"}
	}
	if out.Version == "" {
		return ClientTestResult{Ok: false, Error: "api_failed"}
	}
	return ClientTestResult{Ok: true, Details: map[string]string{"version": out.Version}}
}
