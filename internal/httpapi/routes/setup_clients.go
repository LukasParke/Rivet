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
	"github.com/LukeHagar/Rivet/internal/ws"
)

type SetupClientsDeps struct {
	DB *sql.DB

	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
	Hub       *ws.Hub
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
		var actor *string
		if u, ok := middleware.UserFromContext(c); ok {
			actor = &u.ID
		}
		_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{Action: "setup.clients_saved", ActorUserID: actor})
		emitAudit(deps.Hub, "setup.clients_saved", actor, nil)

		return c.JSON(SetupClientsSaveResponse{Saved: true})
	})
}

func validateClients(ctx context.Context, deps SetupClientsDeps, req SetupClientsValidateRequest) SetupClientsValidateResponse {
	resp := SetupClientsValidateResponse{Ok: true}

	// Require at least one configured client.
	if req.QBittorrent == nil && req.NZBGet == nil && req.SABnzbd == nil {
		if deps.Hub != nil {
			deps.Hub.Publish("setup.clients.validation.error", map[string]any{
				"error":   "no_clients_provided",
				"message": "At least one client must be configured",
			})
		}
		return SetupClientsValidateResponse{Ok: false}
	}

	if deps.Hub != nil {
		clientCount := 0
		if req.QBittorrent != nil {
			clientCount++
		}
		if req.NZBGet != nil {
			clientCount++
		}
		if req.SABnzbd != nil {
			clientCount++
		}
		deps.Hub.Publish("setup.clients.validation.started", map[string]any{
			"qbittorrent": req.QBittorrent != nil,
			"nzbget":      req.NZBGet != nil,
			"sabnzbd":     req.SABnzbd != nil,
			"totalCount":  clientCount,
		})
	}

	client := &http.Client{Timeout: 6 * time.Second}

	if req.QBittorrent != nil {
		if deps.Hub != nil {
			deps.Hub.Publish("setup.clients.validation.progress", map[string]any{
				"client": "qbittorrent",
				"status": "testing",
			})
		}
		r := testQBittorrent(ctx, client, deps.Policy, deps.Hub, *req.QBittorrent)
		resp.QBittorrent = &r
		resp.Ok = resp.Ok && r.Ok
		if deps.Hub != nil {
			deps.Hub.Publish("setup.clients.validation.progress", map[string]any{
				"client": "qbittorrent",
				"status": "completed",
				"ok":     r.Ok,
				"error":   r.Error,
				"details": r.Details,
			})
		}
	}
	if req.NZBGet != nil {
		if deps.Hub != nil {
			deps.Hub.Publish("setup.clients.validation.progress", map[string]any{
				"client": "nzbget",
				"status": "testing",
			})
		}
		r := testNZBGet(ctx, client, deps.Policy, deps.Hub, *req.NZBGet)
		resp.NZBGet = &r
		resp.Ok = resp.Ok && r.Ok
		if deps.Hub != nil {
			deps.Hub.Publish("setup.clients.validation.progress", map[string]any{
				"client": "nzbget",
				"status": "completed",
				"ok":     r.Ok,
				"error":   r.Error,
				"details": r.Details,
			})
		}
	}
	if req.SABnzbd != nil {
		if deps.Hub != nil {
			deps.Hub.Publish("setup.clients.validation.progress", map[string]any{
				"client": "sabnzbd",
				"status": "testing",
			})
		}
		r := testSABnzbd(ctx, client, deps.Policy, deps.Hub, *req.SABnzbd)
		resp.SABnzbd = &r
		resp.Ok = resp.Ok && r.Ok
		if deps.Hub != nil {
			deps.Hub.Publish("setup.clients.validation.progress", map[string]any{
				"client": "sabnzbd",
				"status": "completed",
				"ok":     r.Ok,
				"error":   r.Error,
				"details": r.Details,
			})
		}
	}

	if deps.Hub != nil {
		deps.Hub.Publish("setup.clients.validation.completed", map[string]any{
			"ok": resp.Ok,
			"summary": map[string]any{
				"qbittorrentOk": resp.QBittorrent != nil && resp.QBittorrent.Ok,
				"nzbgetOk":      resp.NZBGet != nil && resp.NZBGet.Ok,
				"sabnzbdOk":     resp.SABnzbd != nil && resp.SABnzbd.Ok,
			},
		})
	}

	return resp
}

func testQBittorrent(ctx context.Context, client *http.Client, p security.OutboundPolicy, hub *ws.Hub, cfg QBittorrentConfig) ClientTestResult {
	cfg.URL = strings.TrimSpace(cfg.URL)
	cfg.Username = strings.TrimSpace(cfg.Username)
	cfg.Password = strings.TrimSpace(cfg.Password)
	if cfg.URL == "" || cfg.Username == "" || cfg.Password == "" {
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "qbittorrent",
				"error":     "missing_fields",
				"errorCode": "missing_fields",
			})
		}
		return ClientTestResult{Ok: false, Error: "missing_fields"}
	}

	base, err := security.ValidateOutboundURL(cfg.URL, p)
	if err != nil {
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "qbittorrent",
				"error":     err.Error(),
				"errorCode": "invalid_url",
			})
		}
		return ClientTestResult{Ok: false, Error: err.Error()}
	}

	if hub != nil {
		hub.Publish("setup.clients.test.connecting", map[string]any{
			"client": "qbittorrent",
			"url":    cfg.URL,
		})
	}

	// Login (sets cookie)
	loginURL := base.ResolveReference(&url.URL{Path: "/api/v2/auth/login"})
	form := url.Values{}
	form.Set("username", cfg.Username)
	form.Set("password", cfg.Password)

	if hub != nil {
		hub.Publish("setup.clients.test.authenticating", map[string]any{
			"client": "qbittorrent",
		})
	}
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, loginURL.String(), strings.NewReader(form.Encode()))
	req.Header.Set("content-type", "application/x-www-form-urlencoded")
	res, err := client.Do(req)
	if err != nil {
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "qbittorrent",
				"error":     "connect_failed",
				"errorCode": "connect_failed",
			})
		}
		return ClientTestResult{Ok: false, Error: "connect_failed"}
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(res.Body, 1024))
	if res.StatusCode != 200 {
		result := ClientTestResult{Ok: false, Error: "login_failed"}
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "qbittorrent",
				"error":     result.Error,
				"errorCode": "login_failed",
				"statusCode": res.StatusCode,
			})
		}
		return result
	}
	if strings.TrimSpace(string(b)) != "Ok." {
		// qBittorrent returns Ok. on success.
		result := ClientTestResult{Ok: false, Error: "login_failed"}
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "qbittorrent",
				"error":     result.Error,
				"errorCode": "login_failed",
			})
		}
		return result
	}

	cookie := res.Header.Get("set-cookie")
	if cookie == "" {
		// still try version call; some proxies strip headers
	}

	if hub != nil {
		hub.Publish("setup.clients.test.querying", map[string]any{
			"client": "qbittorrent",
			"query":  "version",
		})
	}
	verURL := base.ResolveReference(&url.URL{Path: "/api/v2/app/version"})
	vreq, _ := http.NewRequestWithContext(ctx, http.MethodGet, verURL.String(), nil)
	if cookie != "" {
		vreq.Header.Set("cookie", cookie)
	}
	vres, err := client.Do(vreq)
	if err != nil {
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "qbittorrent",
				"error":     "version_failed",
				"errorCode": "version_failed",
			})
		}
		return ClientTestResult{Ok: false, Error: "version_failed"}
	}
	defer vres.Body.Close()
	vb, _ := io.ReadAll(io.LimitReader(vres.Body, 1024))
	if vres.StatusCode != 200 {
		result := ClientTestResult{Ok: false, Error: "version_failed"}
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "qbittorrent",
				"error":     result.Error,
				"errorCode": "version_failed",
				"statusCode": vres.StatusCode,
			})
		}
		return result
	}

	result := ClientTestResult{Ok: true, Details: map[string]string{"version": strings.TrimSpace(string(vb))}}
	if hub != nil {
		hub.Publish("setup.clients.test.completed", map[string]any{
			"client":    "qbittorrent",
			"ok":        true,
			"version":   strings.TrimSpace(string(vb)),
			"details":   result.Details,
		})
	}
	return result
}

func testNZBGet(ctx context.Context, client *http.Client, p security.OutboundPolicy, hub *ws.Hub, cfg NZBGetConfig) ClientTestResult {
	cfg.URL = strings.TrimSpace(cfg.URL)
	cfg.Username = strings.TrimSpace(cfg.Username)
	cfg.Password = strings.TrimSpace(cfg.Password)
	if cfg.URL == "" || cfg.Username == "" || cfg.Password == "" {
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "nzbget",
				"error":     "missing_fields",
				"errorCode": "missing_fields",
			})
		}
		return ClientTestResult{Ok: false, Error: "missing_fields"}
	}

	base, err := security.ValidateOutboundURL(cfg.URL, p)
	if err != nil {
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "nzbget",
				"error":     err.Error(),
				"errorCode": "invalid_url",
			})
		}
		return ClientTestResult{Ok: false, Error: err.Error()}
	}

	if hub != nil {
		hub.Publish("setup.clients.test.connecting", map[string]any{
			"client": "nzbget",
			"url":    cfg.URL,
		})
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
	if hub != nil {
		hub.Publish("setup.clients.test.querying", map[string]any{
			"client": "nzbget",
			"query":  "version",
		})
	}
	res, err := client.Do(req)
	if err != nil {
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "nzbget",
				"error":     "connect_failed",
				"errorCode": "connect_failed",
			})
		}
		return ClientTestResult{Ok: false, Error: "connect_failed"}
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		result := ClientTestResult{Ok: false, Error: "auth_failed"}
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "nzbget",
				"error":     result.Error,
				"errorCode": "auth_failed",
				"statusCode": res.StatusCode,
			})
		}
		return result
	}

	respB, _ := io.ReadAll(io.LimitReader(res.Body, 4096))
	var out struct {
		Result any `json:"result"`
		Error  any `json:"error"`
	}
	_ = json.Unmarshal(respB, &out)
	if out.Error != nil {
		result := ClientTestResult{Ok: false, Error: "rpc_error"}
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "nzbget",
				"error":     result.Error,
				"errorCode": "rpc_error",
			})
		}
		return result
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
	result := ClientTestResult{Ok: true, Details: map[string]string{"version": ver}}
	if hub != nil {
		hub.Publish("setup.clients.test.completed", map[string]any{
			"client":  "nzbget",
			"ok":      true,
			"version": ver,
			"details": result.Details,
		})
	}
	return result
}

func testSABnzbd(ctx context.Context, client *http.Client, p security.OutboundPolicy, hub *ws.Hub, cfg SABnzbdConfig) ClientTestResult {
	cfg.URL = strings.TrimSpace(cfg.URL)
	cfg.APIKey = strings.TrimSpace(cfg.APIKey)
	if cfg.URL == "" || cfg.APIKey == "" {
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "sabnzbd",
				"error":     "missing_fields",
				"errorCode": "missing_fields",
			})
		}
		return ClientTestResult{Ok: false, Error: "missing_fields"}
	}

	base, err := security.ValidateOutboundURL(cfg.URL, p)
	if err != nil {
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "sabnzbd",
				"error":     err.Error(),
				"errorCode": "invalid_url",
			})
		}
		return ClientTestResult{Ok: false, Error: err.Error()}
	}

	if hub != nil {
		hub.Publish("setup.clients.test.connecting", map[string]any{
			"client": "sabnzbd",
			"url":    cfg.URL,
		})
	}

	// SABnzbd API: GET /api?mode=version&output=json&apikey=...
	apiURL := base.ResolveReference(&url.URL{Path: "/api"})
	q := apiURL.Query()
	q.Set("mode", "version")
	q.Set("output", "json")
	q.Set("apikey", cfg.APIKey)
	apiURL.RawQuery = q.Encode()

	if hub != nil {
		hub.Publish("setup.clients.test.querying", map[string]any{
			"client": "sabnzbd",
			"query":  "version",
		})
	}
	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, apiURL.String(), nil)
	res, err := client.Do(req)
	if err != nil {
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "sabnzbd",
				"error":     "connect_failed",
				"errorCode": "connect_failed",
			})
		}
		return ClientTestResult{Ok: false, Error: "connect_failed"}
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		result := ClientTestResult{Ok: false, Error: "api_failed"}
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "sabnzbd",
				"error":     result.Error,
				"errorCode": "api_failed",
				"statusCode": res.StatusCode,
			})
		}
		return result
	}

	respB, _ := io.ReadAll(io.LimitReader(res.Body, 4096))
	var out struct {
		Version string `json:"version"`
	}
	if err := json.Unmarshal(respB, &out); err != nil {
		result := ClientTestResult{Ok: false, Error: "parse_failed"}
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "sabnzbd",
				"error":     result.Error,
				"errorCode": "parse_failed",
			})
		}
		return result
	}
	if out.Version == "" {
		result := ClientTestResult{Ok: false, Error: "api_failed"}
		if hub != nil {
			hub.Publish("setup.clients.test.failed", map[string]any{
				"client":    "sabnzbd",
				"error":     result.Error,
				"errorCode": "api_failed",
			})
		}
		return result
	}
	result := ClientTestResult{Ok: true, Details: map[string]string{"version": out.Version}}
	if hub != nil {
		hub.Publish("setup.clients.test.completed", map[string]any{
			"client":  "sabnzbd",
			"ok":      true,
			"version": out.Version,
			"details": result.Details,
		})
	}
	return result
}
