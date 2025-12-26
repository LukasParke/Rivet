package routes

import (
	"context"
	"database/sql"
	"encoding/json"
	"encoding/xml"
	"errors"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	netutil "github.com/LukeHagar/Rivet/internal/network"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
	"github.com/LukeHagar/Rivet/internal/ws"
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
	Hub       *ws.Hub
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
		var actor *string
		if u, ok := middleware.UserFromContext(c); ok {
			actor = &u.ID
		}
		_ = storage.InsertAuditEvent(ctx, deps.DB, storage.AuditEvent{Action: "setup.indexers_saved", ActorUserID: actor})
		emitAudit(deps.Hub, "setup.indexers_saved", actor, nil)

		return c.JSON(SetupIndexersSaveResponse{Saved: true})
	})
}

func validateIndexers(ctx context.Context, deps SetupIndexersDeps, req SetupIndexersValidateRequest) SetupIndexersValidateResponse {
	resp := SetupIndexersValidateResponse{Ok: true}

	if len(req.Torznab) == 0 && len(req.Newznab) == 0 {
		resp.Ok = false
		if deps.Hub != nil {
			deps.Hub.Publish("setup.indexer.validation.error", map[string]any{
				"error": "no_indexers_provided",
				"message": "At least one indexer (Torznab or Newznab) must be provided",
			})
		}
		return resp
	}

	totalCount := len(req.Torznab) + len(req.Newznab)
	if deps.Hub != nil {
		deps.Hub.Publish("setup.indexer.validation.started", map[string]any{
			"torznabCount": len(req.Torznab),
			"newznabCount": len(req.Newznab),
			"totalCount":   totalCount,
		})
	}

	// Use custom Transport with Dialer that binds to appropriate source IP
	// This fixes "no route to host" issues on macOS when curl works but Go doesn't
	client := &http.Client{
		Timeout: 8 * time.Second,
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, network, addr string) (net.Conn, error) {
				return netutil.DialWithSourceIP(addr)
			},
			MaxIdleConns:          10,
			IdleConnTimeout:       90 * time.Second,
			TLSHandshakeTimeout:   5 * time.Second,
			ExpectContinueTimeout: 1 * time.Second,
		},
	}

	resp.Torznab = make([]IndexerTestResult, 0, len(req.Torznab))
	for i, ix := range req.Torznab {
		if deps.Hub != nil {
			deps.Hub.Publish("setup.indexer.validation.progress", map[string]any{
				"step":        i + 1,
				"total":       totalCount,
				"indexerName": ix.Name,
				"indexerUrl":  ix.URL,
				"type":        "torznab",
				"status":      "testing",
			})
		}
		r := testNewznabLike(ctx, client, deps.Policy, deps.Hub, ix)
		resp.Torznab = append(resp.Torznab, r)
		resp.Ok = resp.Ok && r.Ok
		if deps.Hub != nil {
			deps.Hub.Publish("setup.indexer.validation.progress", map[string]any{
				"step":        i + 1,
				"total":       totalCount,
				"indexerName": ix.Name,
				"indexerUrl":  ix.URL,
				"type":        "torznab",
				"status":      "completed",
				"ok":          r.Ok,
				"error":       r.Error,
				"details":     r.Details,
			})
		}
	}
	resp.Newznab = make([]IndexerTestResult, 0, len(req.Newznab))
	for i, ix := range req.Newznab {
		if deps.Hub != nil {
			deps.Hub.Publish("setup.indexer.validation.progress", map[string]any{
				"step":        len(req.Torznab) + i + 1,
				"total":       totalCount,
				"indexerName": ix.Name,
				"indexerUrl":  ix.URL,
				"type":        "newznab",
				"status":      "testing",
			})
		}
		r := testNewznabLike(ctx, client, deps.Policy, deps.Hub, ix)
		resp.Newznab = append(resp.Newznab, r)
		resp.Ok = resp.Ok && r.Ok
		if deps.Hub != nil {
			deps.Hub.Publish("setup.indexer.validation.progress", map[string]any{
				"step":        len(req.Torznab) + i + 1,
				"total":       totalCount,
				"indexerName": ix.Name,
				"indexerUrl":  ix.URL,
				"type":        "newznab",
				"status":      "completed",
				"ok":          r.Ok,
				"error":       r.Error,
				"details":     r.Details,
			})
		}
	}

	if deps.Hub != nil {
		deps.Hub.Publish("setup.indexer.validation.completed", map[string]any{
			"ok":        resp.Ok,
			"torznabOk": len(resp.Torznab) > 0 && allOk(resp.Torznab),
			"newznabOk": len(resp.Newznab) > 0 && allOk(resp.Newznab),
			"summary": map[string]any{
				"torznabCount": len(resp.Torznab),
				"newznabCount": len(resp.Newznab),
				"torznabResults": resp.Torznab,
				"newznabResults": resp.Newznab,
			},
		})
	}

	return resp
}

func allOk(results []IndexerTestResult) bool {
	for _, r := range results {
		if !r.Ok {
			return false
		}
	}
	return true
}

// isProwlarrURL detects if the URL is likely a Prowlarr instance.
func isProwlarrURL(urlStr string) bool {
	urlStr = strings.ToLower(urlStr)
	return strings.Contains(urlStr, "prowlarr") || 
		strings.Contains(urlStr, ":9696") ||
		strings.Contains(urlStr, "/prowlarr")
}

// Torznab and Newznab are both Newznab-style RSS endpoints.
func testNewznabLike(ctx context.Context, client *http.Client, p security.OutboundPolicy, hub *ws.Hub, cfg IndexerConfig) IndexerTestResult {
	cfg.URL = strings.TrimSpace(cfg.URL)
	cfg.APIKey = strings.TrimSpace(cfg.APIKey)
	cfg.Username = strings.TrimSpace(cfg.Username)
	cfg.Password = strings.TrimSpace(cfg.Password)

	if cfg.URL == "" || cfg.APIKey == "" {
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       "missing_fields",
				"errorCode":   "missing_fields",
				"details":     map[string]string{"hint": "URL and API key are required"},
			})
		}
		return IndexerTestResult{Ok: false, Error: "missing_fields"}
	}

	if hub != nil {
		hub.Publish("setup.indexer.test.started", map[string]any{
			"indexerName": cfg.Name,
			"indexerUrl":  cfg.URL,
		})
	}

	base, err := security.ValidateOutboundURL(cfg.URL, p)
	if err != nil {
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       err.Error(),
				"errorCode":   "invalid_url",
				"details":     map[string]string{"hint": "Invalid or disallowed URL"},
			})
		}
		return IndexerTestResult{Ok: false, Error: err.Error()}
	}

	// Check if this is Prowlarr and provide helpful error messages.
	isProwlarr := isProwlarrURL(cfg.URL)
	if isProwlarr {
		if hub != nil {
			hub.Publish("setup.indexer.prowlarr.detected", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
			})
		}
		// For Prowlarr, directly find an enabled indexer and use its Torznab endpoint
		// instead of trying the generic /api endpoint which returns JSON API info
		if hub != nil {
			hub.Publish("setup.indexer.prowlarr.finding_indexer", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
			})
		}
		indexerID, err := findProwlarrIndexer(ctx, client, base, cfg.APIKey, hub)
		if err != nil {
			// Check if it's an authentication error
			if err.Error() == "prowlarr_api_failed" {
				result := IndexerTestResult{
					Ok:    false,
					Error: "invalid_prowlarr_api_key",
					Details: map[string]string{
						"hint": "Get your API key from Prowlarr Settings → General → API Key",
					},
				}
				if hub != nil {
					hub.Publish("setup.indexer.test.failed", map[string]any{
						"indexerName": cfg.Name,
						"indexerUrl":  cfg.URL,
						"error":       result.Error,
						"errorCode":   "invalid_prowlarr_api_key",
						"errorType":   "auth",
						"details":     result.Details,
					})
				}
				return result
			}
			// If we can't find an indexer, return helpful error
			if err.Error() == "no_enabled_indexers" {
				result := IndexerTestResult{
					Ok:    false,
					Error: "prowlarr_no_indexers",
					Details: map[string]string{
						"hint": "No enabled indexers found in Prowlarr. Enable at least one indexer in Prowlarr's web UI, then try again.",
					},
				}
				if hub != nil {
					hub.Publish("setup.indexer.test.failed", map[string]any{
						"indexerName": cfg.Name,
						"indexerUrl":  cfg.URL,
						"error":       result.Error,
						"errorCode":   "prowlarr_no_indexers",
						"errorType":   "configuration",
						"details":      result.Details,
					})
				}
				return result
			}
			// Connection or other errors - use network error analysis
			errorType, hint := networkErrorInfo(err, cfg.URL)
			result := IndexerTestResult{
				Ok:    false,
				Error: "prowlarr_connection_failed",
				Details: map[string]string{
					"hint": hint,
				},
			}
			if hub != nil {
				hub.Publish("setup.indexer.test.failed", map[string]any{
					"indexerName": cfg.Name,
					"indexerUrl":  cfg.URL,
					"error":       result.Error,
					"errorCode":   "prowlarr_connection_failed",
					"errorType":   errorType,
					"rawError":    err.Error(),
					"details":     result.Details,
				})
			}
			return result
		}
		// Found an enabled indexer, use the indexer-specific Torznab endpoint
		if hub != nil {
			hub.Publish("setup.indexer.prowlarr.indexer_found", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"indexerId":   indexerID,
			})
		}
		return retryProwlarrTorznab(ctx, client, base, cfg, indexerID, hub)
	}

	// For non-Prowlarr Torznab/Newznab indexers, use the standard endpoint
	if hub != nil {
		hub.Publish("setup.indexer.test.connecting", map[string]any{
			"indexerName": cfg.Name,
			"indexerUrl":  cfg.URL,
		})
	}
	apiURL := base.ResolveReference(&url.URL{Path: "/api"})
	q := apiURL.Query()
	q.Set("t", "search")
	q.Set("q", "ubuntu")
	q.Set("apikey", cfg.APIKey)
	q.Set("limit", "1")
	apiURL.RawQuery = q.Encode()

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, apiURL.String(), nil)
	// Add X-Api-Key header for better compatibility
	req.Header.Set("X-Api-Key", cfg.APIKey)
	if cfg.Username != "" || cfg.Password != "" {
		if hub != nil {
			hub.Publish("setup.indexer.test.authenticating", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"hasUsername": cfg.Username != "",
			})
		}
		req.SetBasicAuth(cfg.Username, cfg.Password)
	}
	if hub != nil {
		hub.Publish("setup.indexer.test.querying", map[string]any{
			"indexerName": cfg.Name,
			"indexerUrl":  cfg.URL,
			"query":       "ubuntu",
		})
	}
	res, err := client.Do(req)
	if err != nil {
		errorType, hint := networkErrorInfo(err, base.String())
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       "connect_failed",
				"errorCode":   "connect_failed",
				"errorType":   errorType,
				"rawError":    err.Error(),
				"details":     map[string]string{"hint": hint},
			})
		}
		return IndexerTestResult{Ok: false, Error: "connect_failed"}
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		if res.StatusCode == 401 || res.StatusCode == 403 {
			result := IndexerTestResult{
				Ok:    false,
				Error: "invalid_api_key",
				Details: map[string]string{
					"hint": "Check your API key and ensure it's correct.",
				},
			}
			if hub != nil {
				hub.Publish("setup.indexer.test.failed", map[string]any{
					"indexerName": cfg.Name,
					"indexerUrl":  cfg.URL,
					"error":       result.Error,
					"errorCode":   "invalid_api_key",
					"errorType":   "auth",
					"details":     result.Details,
					"statusCode":  res.StatusCode,
				})
			}
			return result
		}
		result := IndexerTestResult{Ok: false, Error: "http_" + itoa(res.StatusCode)}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "http_error",
				"errorType":   "http",
				"statusCode":  res.StatusCode,
			})
		}
		return result
	}

	body, _ := io.ReadAll(io.LimitReader(res.Body, 1<<20))

	// Check if body looks like XML/RSS before parsing
	bodyStr := strings.TrimSpace(string(body))
	if bodyStr == "" {
		result := IndexerTestResult{Ok: false, Error: "empty_response"}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "empty_response",
				"details":     map[string]string{"hint": "Indexer returned empty response"},
			})
		}
		return result
	}

	// Check if response is JSON (unexpected for Torznab/Newznab)
	if strings.HasPrefix(bodyStr, "{") || strings.HasPrefix(bodyStr, "[") {
		preview := bodyStr
		if len(preview) > 200 {
			preview = preview[:200] + "..."
		}
		result := IndexerTestResult{Ok: false, Error: "json_response_expected_xml", Details: map[string]string{"preview": preview}}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "json_response_expected_xml",
				"details":     result.Details,
			})
		}
		return result
	}

	// Try parse as Newznab error response (sometimes it's root <error .../>).
	if errDesc, ok := parseNewznabError(body); ok {
		result := IndexerTestResult{Ok: false, Error: errDesc}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "newznab_error",
				"details":     map[string]string{"hint": errDesc},
			})
		}
		return result
	}

	// Check if it's valid XML
	if !strings.HasPrefix(bodyStr, "<?xml") && !strings.HasPrefix(bodyStr, "<rss") && !strings.HasPrefix(bodyStr, "<error") {
		preview := bodyStr
		if len(preview) > 200 {
			preview = preview[:200] + "..."
		}
		result := IndexerTestResult{Ok: false, Error: "invalid_format", Details: map[string]string{"preview": preview}}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "invalid_format",
				"details":     result.Details,
			})
		}
		return result
	}

	if hub != nil {
		hub.Publish("setup.indexer.test.parsing", map[string]any{
			"indexerName": cfg.Name,
			"indexerUrl":  cfg.URL,
		})
	}
	items, err := countRSSItems(body)
	if err != nil {
		result := IndexerTestResult{Ok: false, Error: "parse_failed", Details: map[string]string{"parseErr": err.Error()}}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "parse_failed",
				"details":     result.Details,
			})
		}
		return result
	}
	if items < 0 {
		items = 0
	}

	details := map[string]string{"itemsParsed": itoa(items)}
	result := IndexerTestResult{Ok: true, Details: details}
	if hub != nil {
		hub.Publish("setup.indexer.test.completed", map[string]any{
			"indexerName": cfg.Name,
			"indexerUrl":  cfg.URL,
			"ok":          true,
			"itemsFound":  items,
			"details":     details,
		})
	}
	return result
}

// findProwlarrIndexer queries Prowlarr's REST API to find the first enabled indexer.
// Returns the indexer ID, or an error if none found or API call fails.
func findProwlarrIndexer(ctx context.Context, client *http.Client, base *url.URL, apiKey string, hub *ws.Hub) (int, error) {
	indexersURL := base.ResolveReference(&url.URL{Path: "/api/v1/indexer"})
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, indexersURL.String(), nil)
	if err != nil {
		return 0, err
	}
	req.Header.Set("X-Api-Key", apiKey)

	res, err := client.Do(req)
	if err != nil {
		errorType, hint := networkErrorInfo(err, base.String())
		if hub != nil {
			hub.Publish("setup.indexer.prowlarr.error", map[string]any{
				"error":     "connection_failed",
				"errorCode": "prowlarr_connection_failed",
				"errorType": errorType,
				"rawError":  err.Error(),
				"details":   map[string]string{"hint": hint},
			})
		}
		return 0, err
	}
	defer res.Body.Close()

	if res.StatusCode == 401 || res.StatusCode == 403 {
		if hub != nil {
			hub.Publish("setup.indexer.prowlarr.error", map[string]any{
				"error":     "prowlarr_api_failed",
				"errorCode": "invalid_prowlarr_api_key",
				"errorType": "auth",
				"statusCode": res.StatusCode,
				"details":   map[string]string{"hint": "Invalid API key or unauthorized"},
			})
		}
		return 0, errors.New("prowlarr_api_failed")
	}
	if res.StatusCode != 200 {
		if hub != nil {
			hub.Publish("setup.indexer.prowlarr.error", map[string]any{
				"error":     "prowlarr_api_failed",
				"errorCode": "prowlarr_api_error",
				"errorType": "http",
				"statusCode": res.StatusCode,
				"details":   map[string]string{"hint": "Prowlarr API returned error status"},
			})
		}
		return 0, errors.New("prowlarr_api_failed")
	}

	body, err := io.ReadAll(io.LimitReader(res.Body, 1<<20))
	if err != nil {
		return 0, err
	}

	// Parse the indexer list response
	var indexers []struct {
		ID     int  `json:"id"`
		Enable bool `json:"enable"`
	}
	if err := json.Unmarshal(body, &indexers); err != nil {
		if hub != nil {
			hub.Publish("setup.indexer.prowlarr.error", map[string]any{
				"error":     "parse_failed",
				"errorCode": "prowlarr_parse_failed",
				"errorType": "parse",
				"rawError":  err.Error(),
				"details":   map[string]string{"hint": "Failed to parse Prowlarr response"},
			})
		}
		return 0, err
	}

	// Find the first enabled indexer
	for _, idx := range indexers {
		if idx.Enable {
			return idx.ID, nil
		}
	}

	if hub != nil {
		hub.Publish("setup.indexer.prowlarr.error", map[string]any{
			"error":     "no_enabled_indexers",
			"errorCode": "prowlarr_no_indexers",
			"errorType": "configuration",
			"details":   map[string]string{"hint": "No enabled indexers found in Prowlarr"},
		})
	}
	return 0, errors.New("no_enabled_indexers")
}

// retryProwlarrTorznab retries the Torznab validation using a specific indexer's endpoint.
func retryProwlarrTorznab(ctx context.Context, client *http.Client, base *url.URL, cfg IndexerConfig, indexerID int, hub *ws.Hub) IndexerTestResult {
	// Construct the indexer-specific Torznab endpoint
	endpointPath := "/api/v1/indexer/" + itoa(indexerID) + "/newznab"
	if hub != nil {
		hub.Publish("setup.indexer.prowlarr.using_endpoint", map[string]any{
			"indexerName": cfg.Name,
			"indexerUrl":  cfg.URL,
			"indexerId":   indexerID,
			"endpoint":    endpointPath,
		})
	}
	apiURL := base.ResolveReference(&url.URL{Path: endpointPath})
	q := apiURL.Query()
	q.Set("t", "search")
	q.Set("q", "ubuntu")
	q.Set("apikey", cfg.APIKey)
	q.Set("limit", "1")
	apiURL.RawQuery = q.Encode()

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, apiURL.String(), nil)
	// Use X-Api-Key header for authentication (primary method for Prowlarr)
	req.Header.Set("X-Api-Key", cfg.APIKey)
	if cfg.Username != "" || cfg.Password != "" {
		req.SetBasicAuth(cfg.Username, cfg.Password)
	}

	if hub != nil {
		hub.Publish("setup.indexer.test.querying", map[string]any{
			"indexerName": cfg.Name,
			"indexerUrl":  cfg.URL,
			"query":       "ubuntu",
			"type":        "prowlarr",
		})
	}
	res, err := client.Do(req)
	if err != nil {
		errorType, hint := networkErrorInfo(err, base.String())
		result := IndexerTestResult{
			Ok:    false,
			Error: "prowlarr_connection_failed",
			Details: map[string]string{
				"hint": hint,
			},
		}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "prowlarr_connection_failed",
				"errorType":   errorType,
				"rawError":    err.Error(),
				"details":     result.Details,
			})
		}
		return result
	}
	defer res.Body.Close()

	if res.StatusCode != 200 {
		if res.StatusCode == 401 || res.StatusCode == 403 {
			result := IndexerTestResult{
				Ok:    false,
				Error: "invalid_prowlarr_api_key",
				Details: map[string]string{
					"hint": "Get your API key from Prowlarr Settings → General → API Key",
				},
			}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "invalid_prowlarr_api_key",
				"errorType":   "auth",
				"details":     result.Details,
				"statusCode":  res.StatusCode,
			})
		}
			return result
		}
		result := IndexerTestResult{Ok: false, Error: "http_" + itoa(res.StatusCode)}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "http_error",
				"errorType":   "http",
				"statusCode":  res.StatusCode,
			})
		}
		return result
	}

	body, _ := io.ReadAll(io.LimitReader(res.Body, 1<<20))
	bodyStr := strings.TrimSpace(string(body))

	if bodyStr == "" {
		result := IndexerTestResult{
			Ok:    false,
			Error: "prowlarr_empty_response",
			Details: map[string]string{
				"hint": "Prowlarr returned an empty response. Ensure the indexer is properly configured.",
			},
		}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "prowlarr_empty_response",
				"details":     result.Details,
			})
		}
		return result
	}

	// Check if it's an error response
	if errDesc, ok := parseNewznabError(body); ok {
		result := IndexerTestResult{
			Ok:    false,
			Error: "prowlarr_error",
			Details: map[string]string{
				"hint":     "Prowlarr returned an error. " + errDesc,
				"rawError": errDesc,
			},
		}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "prowlarr_error",
				"details":     result.Details,
			})
		}
		return result
	}

	// Check if it's valid XML
	if !strings.HasPrefix(bodyStr, "<?xml") && !strings.HasPrefix(bodyStr, "<rss") && !strings.HasPrefix(bodyStr, "<error") {
		preview := bodyStr
		if len(preview) > 200 {
			preview = preview[:200] + "..."
		}
		result := IndexerTestResult{
			Ok:    false,
			Error: "prowlarr_invalid_format",
			Details: map[string]string{
				"hint":    "Prowlarr response is not valid XML/RSS. Ensure the indexer is properly configured.",
				"preview": preview,
			},
		}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "prowlarr_invalid_format",
				"details":     result.Details,
			})
		}
		return result
	}

	if hub != nil {
		hub.Publish("setup.indexer.test.parsing", map[string]any{
			"indexerName": cfg.Name,
			"indexerUrl":  cfg.URL,
			"type":       "prowlarr",
		})
	}
	items, err := countRSSItems(body)
	if err != nil {
		result := IndexerTestResult{
			Ok:    false,
			Error: "prowlarr_parse_failed",
			Details: map[string]string{
				"hint": "Failed to parse Prowlarr response. Ensure the indexer is properly configured.",
			},
		}
		if hub != nil {
			hub.Publish("setup.indexer.test.failed", map[string]any{
				"indexerName": cfg.Name,
				"indexerUrl":  cfg.URL,
				"error":       result.Error,
				"errorCode":   "prowlarr_parse_failed",
				"details":     result.Details,
			})
		}
		return result
	}
	if items < 0 {
		items = 0
	}

	details := map[string]string{
		"itemsParsed": itoa(items),
		"type":        "prowlarr",
		"note":        "Prowlarr detected - using indexer ID " + itoa(indexerID),
	}

	result := IndexerTestResult{Ok: true, Details: details}
	if hub != nil {
		hub.Publish("setup.indexer.test.completed", map[string]any{
			"indexerName": cfg.Name,
			"indexerUrl":  cfg.URL,
			"ok":          true,
			"itemsFound":  items,
			"details":     details,
			"type":        "prowlarr",
		})
	}
	return result
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

// networkErrorInfo analyzes a network error and returns diagnostic information
func networkErrorInfo(err error, targetURL string) (errorType string, hint string) {
	if err == nil {
		return "", ""
	}
	errStr := strings.ToLower(err.Error())
	
	// Check for specific network error patterns
	if strings.Contains(errStr, "no route to host") || strings.Contains(errStr, "network is unreachable") {
		return "network", "Network connectivity issue: The server cannot reach " + targetURL + ". This may be because the server is running in Docker and cannot access the host network, or there is a firewall/routing issue. The URL and API key may be valid, but the server's network context cannot route to this address."
	}
	if strings.Contains(errStr, "connection refused") {
		return "network", "Connection refused: The server can reach " + targetURL + " but the connection was refused. This usually means the service is not running on that address/port, or a firewall is blocking the connection."
	}
	if strings.Contains(errStr, "timeout") || strings.Contains(errStr, "deadline exceeded") {
		return "network", "Connection timeout: The server attempted to connect to " + targetURL + " but the connection timed out. This may indicate network latency, firewall issues, or the service is not responding."
	}
	if strings.Contains(errStr, "connection reset") {
		return "network", "Connection reset: The connection to " + targetURL + " was reset. This may indicate the service is rejecting connections or there is a network issue."
	}
	if strings.Contains(errStr, "dial tcp") {
		return "network", "Network connectivity issue: The server cannot establish a TCP connection to " + targetURL + ". This may be because the server is running in Docker and cannot access the host network, or there is a firewall/routing issue. The URL and API key may be valid, but the server's network context cannot route to this address."
	}
	
	// Check for DNS errors
	if strings.Contains(errStr, "no such host") || strings.Contains(errStr, "dns") {
		return "dns", "DNS resolution failed: The server cannot resolve the hostname for " + targetURL + ". Check that the hostname is correct and DNS is working."
	}
	
	// Check for TLS/SSL errors
	if strings.Contains(errStr, "tls") || strings.Contains(errStr, "ssl") || strings.Contains(errStr, "certificate") {
		return "tls", "TLS/SSL error: There was a problem with the SSL/TLS connection to " + targetURL + ". Check certificate configuration."
	}
	
	// Generic network error
	if _, ok := err.(*net.OpError); ok {
		return "network", "Network error: The server encountered a network error while connecting to " + targetURL + ". The URL and API key may be valid, but the server's network context cannot reach this address. Check network configuration, firewall rules, and whether the server is running in a container that needs special network settings."
	}
	
	return "unknown", "Connection failed: " + err.Error()
}
