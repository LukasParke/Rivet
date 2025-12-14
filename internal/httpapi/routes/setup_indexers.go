package routes

import (
	"context"
	"database/sql"
	"encoding/json"
	"encoding/xml"
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

type IndexerConfig struct {
	Name     string `json:"name"`
	URL      string `json:"url"`
	APIKey   string `json:"apiKey"`
	Username string `json:"username,omitempty"`
	Password string `json:"password,omitempty"`
}

type SetupIndexersDeps struct {
	DB *sql.DB

	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
}

type SetupIndexersValidateRequest struct {
	Torznab []IndexerConfig `json:"torznab"`
	Newznab []IndexerConfig `json:"newznab"`
}

type IndexerTestResult struct {
	Ok      bool              `json:"ok"`
	Details map[string]string `json:"details,omitempty"`
	Error   string            `json:"error,omitempty"`
}

type SetupIndexersValidateResponse struct {
	Ok bool `json:"ok"`

	Torznab []IndexerTestResult `json:"torznab"`
	Newznab []IndexerTestResult `json:"newznab"`
}

type SetupIndexersSaveResponse struct {
	Saved bool `json:"saved"`
}

func RegisterSetupIndexers(api fiber.Router, deps SetupIndexersDeps) {
	grp := api.Group("/setup")

	grp.Post("/indexers/validate", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupIndexersValidateRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}
		ctx, cancel := context.WithTimeout(c.UserContext(), 15*time.Second)
		defer cancel()

		resp := validateIndexers(ctx, deps, req)
		return c.JSON(resp)
	})

	grp.Post("/indexers/save", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req SetupIndexersValidateRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}
		ctx, cancel := context.WithTimeout(c.UserContext(), 15*time.Second)
		defer cancel()

		resp := validateIndexers(ctx, deps, req)
		if !resp.Ok {
			return c.Status(http.StatusBadRequest).JSON(resp)
		}

		persist := map[string]any{
			"torznab": []any{},
			"newznab": []any{},
		}

		for _, ix := range req.Torznab {
			keyEnc, _ := deps.SecretBox.EncryptToString([]byte(strings.TrimSpace(ix.APIKey)))
			pwEnc, _ := deps.SecretBox.EncryptToString([]byte(strings.TrimSpace(ix.Password)))
			persist["torznab"] = append(persist["torznab"].([]any), map[string]any{
				"name":        strings.TrimSpace(ix.Name),
				"url":         strings.TrimSpace(ix.URL),
				"apiKeyEnc":   keyEnc,
				"username":    strings.TrimSpace(ix.Username),
				"passwordEnc": pwEnc,
			})
		}
		for _, ix := range req.Newznab {
			keyEnc, _ := deps.SecretBox.EncryptToString([]byte(strings.TrimSpace(ix.APIKey)))
			pwEnc, _ := deps.SecretBox.EncryptToString([]byte(strings.TrimSpace(ix.Password)))
			persist["newznab"] = append(persist["newznab"].([]any), map[string]any{
				"name":        strings.TrimSpace(ix.Name),
				"url":         strings.TrimSpace(ix.URL),
				"apiKeyEnc":   keyEnc,
				"username":    strings.TrimSpace(ix.Username),
				"passwordEnc": pwEnc,
			})
		}

		b, _ := json.Marshal(persist)
		if err := storage.UpsertSetting(ctx, deps.DB, "setup.indexers", string(b)); err != nil {
			return c.Status(http.StatusInternalServerError).JSON(fiber.Map{"error": "save_failed"})
		}
		_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{Action: "setup.indexers_saved"})

		return c.JSON(SetupIndexersSaveResponse{Saved: true})
	})
}

func validateIndexers(ctx context.Context, deps SetupIndexersDeps, req SetupIndexersValidateRequest) SetupIndexersValidateResponse {
	resp := SetupIndexersValidateResponse{Ok: true}

	if len(req.Torznab) == 0 && len(req.Newznab) == 0 {
		resp.Ok = false
		return resp
	}

	client := &http.Client{Timeout: 8 * time.Second}

	resp.Torznab = make([]IndexerTestResult, 0, len(req.Torznab))
	for _, ix := range req.Torznab {
		r := testNewznabLike(ctx, client, deps.Policy, ix)
		resp.Torznab = append(resp.Torznab, r)
		resp.Ok = resp.Ok && r.Ok
	}
	resp.Newznab = make([]IndexerTestResult, 0, len(req.Newznab))
	for _, ix := range req.Newznab {
		r := testNewznabLike(ctx, client, deps.Policy, ix)
		resp.Newznab = append(resp.Newznab, r)
		resp.Ok = resp.Ok && r.Ok
	}

	return resp
}

// Torznab and Newznab are both Newznab-style RSS endpoints.
func testNewznabLike(ctx context.Context, client *http.Client, p security.OutboundPolicy, cfg IndexerConfig) IndexerTestResult {
	cfg.URL = strings.TrimSpace(cfg.URL)
	cfg.APIKey = strings.TrimSpace(cfg.APIKey)
	cfg.Username = strings.TrimSpace(cfg.Username)
	cfg.Password = strings.TrimSpace(cfg.Password)

	if cfg.URL == "" || cfg.APIKey == "" {
		return IndexerTestResult{Ok: false, Error: "missing_fields"}
	}

	base, err := security.ValidateOutboundURL(cfg.URL, p)
	if err != nil {
		return IndexerTestResult{Ok: false, Error: err.Error()}
	}

	apiURL := base.ResolveReference(&url.URL{Path: "/api"})
	q := apiURL.Query()
	q.Set("t", "search")
	q.Set("q", "ubuntu")
	q.Set("apikey", cfg.APIKey)
	q.Set("limit", "1")
	apiURL.RawQuery = q.Encode()

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, apiURL.String(), nil)
	if cfg.Username != "" || cfg.Password != "" {
		req.SetBasicAuth(cfg.Username, cfg.Password)
	}
	res, err := client.Do(req)
	if err != nil {
		return IndexerTestResult{Ok: false, Error: "connect_failed"}
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		return IndexerTestResult{Ok: false, Error: "http_" + itoa(res.StatusCode)}
	}

	body, _ := io.ReadAll(io.LimitReader(res.Body, 1<<20))

	// Try parse as Newznab error response (sometimes it's root <error .../>).
	if errDesc, ok := parseNewznabError(body); ok {
		return IndexerTestResult{Ok: false, Error: errDesc}
	}

	items, err := countRSSItems(body)
	if err != nil {
		return IndexerTestResult{Ok: false, Error: "parse_failed"}
	}
	if items < 0 {
		items = 0
	}

	return IndexerTestResult{Ok: true, Details: map[string]string{"itemsParsed": itoa(items)}}
}

func parseNewznabError(b []byte) (string, bool) {
	// Fast check.
	if !bytesContainsInsensitive(b, []byte("<error")) {
		return "", false
	}
	var e struct {
		XMLName xml.Name `xml:"error"`
		Code    string   `xml:"code,attr"`
		Desc    string   `xml:"description,attr"`
	}
	if err := xml.Unmarshal(b, &e); err == nil && (e.Code != "" || e.Desc != "") {
		if e.Desc != "" {
			return "error:" + e.Desc, true
		}
		return "error_code:" + e.Code, true
	}
	// Some servers wrap <error> under <rss>.
	var r struct {
		Error *struct {
			Code string `xml:"code,attr"`
			Desc string `xml:"description,attr"`
		} `xml:"error"`
	}
	if err := xml.Unmarshal(b, &r); err == nil && r.Error != nil {
		if r.Error.Desc != "" {
			return "error:" + r.Error.Desc, true
		}
		if r.Error.Code != "" {
			return "error_code:" + r.Error.Code, true
		}
	}
	return "", false
}

func countRSSItems(b []byte) (int, error) {
	var rss struct {
		Channel struct {
			Items []struct {
				Title string `xml:"title"`
			} `xml:"item"`
		} `xml:"channel"`
	}
	if err := xml.Unmarshal(b, &rss); err != nil {
		return 0, err
	}
	return len(rss.Channel.Items), nil
}

func bytesContainsInsensitive(haystack []byte, needle []byte) bool {
	return strings.Contains(strings.ToLower(string(haystack)), strings.ToLower(string(needle)))
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	neg := false
	if n < 0 {
		neg = true
		n = -n
	}
	buf := make([]byte, 0, 16)
	for n > 0 {
		d := n % 10
		buf = append(buf, byte('0'+d))
		n /= 10
	}
	if neg {
		buf = append(buf, '-')
	}
	for i, j := 0, len(buf)-1; i < j; i, j = i+1, j-1 {
		buf[i], buf[j] = buf[j], buf[i]
	}
	return string(buf)
}
