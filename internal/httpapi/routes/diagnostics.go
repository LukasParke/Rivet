package routes

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/gofiber/fiber/v2"

	"github.com/LukeHagar/Rivet/internal/httpapi/middleware"
	netutil "github.com/LukeHagar/Rivet/internal/network"
	"github.com/LukeHagar/Rivet/internal/observability"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/storage"
)

type DiagnosticsDeps struct {
	DB      *sql.DB
	Metrics *observability.Metrics
	DryRun            bool
	DryRunFixturePath string
	Policy            security.OutboundPolicy
}

type JobErrorRow struct {
	ID        string `json:"id"`
	Kind      string `json:"kind"`
	State     string `json:"state"`
	Attempt   int    `json:"attempt"`
	LastError string `json:"lastError"`
	UpdatedAt string `json:"updatedAt"`
}

type AuditRow struct {
	ID        string  `json:"id"`
	ActorUserID *string `json:"actorUserId,omitempty"`
	Action    string  `json:"action"`
	IP        *string `json:"ip,omitempty"`
	UserAgent *string `json:"userAgent,omitempty"`
	CreatedAt string  `json:"createdAt"`
}

type DiagnosticsResponse struct {
	GeneratedAt string                 `json:"generatedAt"`
	DryRun bool                        `json:"dryRun"`
	DryRunFixturePathSet bool          `json:"dryRunFixturePathSet"`
	Setup       map[string]string      `json:"setup"`
	Settings    map[string]any         `json:"settings"`
	QueueDepth  map[string]int         `json:"queueDepth"`
	RecentJobs  []JobErrorRow          `json:"recentJobs"`
	RecentAudit []AuditRow             `json:"recentAudit"`
	Metrics     map[string]int64       `json:"metrics"`
}

type TestConnectionRequest struct {
	URL    string `json:"url"`
	APIKey string `json:"apiKey"`
}

type TestConnectionResponse struct {
	URL              string            `json:"url"`
	DNSResolution    DNSInfo           `json:"dnsResolution"`
	NetworkInterfaces []InterfaceInfo  `json:"networkInterfaces"`
	TCPTest         TCPTestResult      `json:"tcpTest"`
	HTTPTestDefault HTTPTestResult     `json:"httpTestDefault"`
	HTTPTestCustom  HTTPTestResult     `json:"httpTestCustom"`
	Summary         string             `json:"summary"`
}

type DNSInfo struct {
	Hostname string   `json:"hostname"`
	IPs      []string `json:"ips"`
	Error    string   `json:"error,omitempty"`
}

type InterfaceInfo struct {
	Name string   `json:"name"`
	IPs  []string `json:"ips"`
}

type TCPTestResult struct {
	Success bool   `json:"success"`
	Error   string `json:"error,omitempty"`
	Duration string `json:"duration,omitempty"`
}

type HTTPTestResult struct {
	Success   bool   `json:"success"`
	StatusCode int    `json:"statusCode,omitempty"`
	Error     string `json:"error,omitempty"`
	Duration  string `json:"duration,omitempty"`
}

func RegisterDiagnostics(api fiber.Router, deps DiagnosticsDeps) {
	api.Get("/diagnostics/bundle", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		ctx, cancel := context.WithTimeout(c.UserContext(), 10*time.Second)
		defer cancel()

		setupKeys := []string{"setup.paths", "setup.clients", "setup.indexers", "setup.metadata", "setup.policies", "setup.report.latest", "auth.registration_open"}
		setup := map[string]string{}
		settings := map[string]any{}

		for _, k := range setupKeys {
			v, ok, err := storage.GetSetting(ctx, deps.DB, k)
			if err != nil {
				setup[k] = "error"
				continue
			}
			if !ok {
				setup[k] = "missing"
				continue
			}
			setup[k] = "set"

			// Attempt to decode JSON and redact sensitive fields.
			var obj any
			if json.Unmarshal([]byte(v), &obj) == nil {
				settings[k] = redact(obj)
			} else {
				// For non-JSON values, still avoid leaking.
				if strings.Contains(k, "secret") || strings.Contains(k, "key") {
					settings[k] = "***"
				} else {
					settings[k] = v
				}
			}
		}

		queueDepth, _ := jobCountsByState(ctx, deps.DB)
		recentJobs, _ := recentJobErrors(ctx, deps.DB, 25)
		recentAudit, _ := recentAudit(ctx, deps.DB, 25)

		m := map[string]int64{}
		if deps.Metrics != nil {
			m["jobs_enqueued_total"] = deps.Metrics.JobsEnqueued.Load()
			m["jobs_claimed_total"] = deps.Metrics.JobsClaimed.Load()
			m["jobs_completed_total"] = deps.Metrics.JobsCompleted.Load()
			m["jobs_retry_scheduled_total"] = deps.Metrics.JobsRetryScheduled.Load()
			m["jobs_failed_total"] = deps.Metrics.JobsFailed.Load()
			m["search_runs_started_total"] = deps.Metrics.SearchRunsStarted.Load()
			m["search_runs_completed_total"] = deps.Metrics.SearchRunsCompleted.Load()
			m["search_runs_failed_total"] = deps.Metrics.SearchRunsFailed.Load()
			m["candidates_recorded_total"] = deps.Metrics.CandidatesRecorded.Load()
			m["decisions_recorded_total"] = deps.Metrics.DecisionsRecorded.Load()
		}

		return c.JSON(DiagnosticsResponse{
			GeneratedAt: time.Now().UTC().Format(time.RFC3339Nano),
			DryRun: deps.DryRun,
			DryRunFixturePathSet: strings.TrimSpace(deps.DryRunFixturePath) != "",
			Setup:       setup,
			Settings:    settings,
			QueueDepth:  queueDepth,
			RecentJobs:  recentJobs,
			RecentAudit: recentAudit,
			Metrics:     m,
		})
	})

	api.Post("/diagnostics/test-connection", middleware.RequireRole("admin"), func(c *fiber.Ctx) error {
		var req TestConnectionRequest
		if err := c.BodyParser(&req); err != nil {
			return c.Status(http.StatusBadRequest).JSON(fiber.Map{"error": "invalid_json"})
		}

		ctx, cancel := context.WithTimeout(c.UserContext(), 15*time.Second)
		defer cancel()

		resp := testConnection(ctx, req.URL, req.APIKey, deps.Policy)
		return c.JSON(resp)
	})
}

func redact(v any) any {
	switch t := v.(type) {
	case map[string]any:
		out := map[string]any{}
		for k, vv := range t {
			lk := strings.ToLower(k)
			if strings.HasSuffix(lk, "enc") || strings.Contains(lk, "password") || strings.Contains(lk, "apikey") || strings.Contains(lk, "token") || strings.Contains(lk, "secret") {
				out[k] = "***"
				continue
			}
			out[k] = redact(vv)
		}
		return out
	case []any:
		out := make([]any, 0, len(t))
		for _, vv := range t {
			out = append(out, redact(vv))
		}
		return out
	default:
		return v
	}
}

func jobCountsByState(ctx context.Context, db *sql.DB) (map[string]int, error) {
	rows, err := db.QueryContext(ctx, `SELECT state, COUNT(1) FROM jobs GROUP BY state`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := map[string]int{}
	for rows.Next() {
		var state string
		var n int
		if err := rows.Scan(&state, &n); err != nil {
			return nil, err
		}
		out[state] = n
	}
	return out, rows.Err()
}

func recentJobErrors(ctx context.Context, db *sql.DB, limit int) ([]JobErrorRow, error) {
	if limit <= 0 || limit > 200 {
		limit = 25
	}
	rows, err := db.QueryContext(ctx, `
SELECT id, kind, state, attempt, COALESCE(last_error,''), updated_at
FROM jobs
WHERE last_error IS NOT NULL AND last_error != ''
ORDER BY updated_at DESC
LIMIT ?
`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []JobErrorRow
	for rows.Next() {
		var r JobErrorRow
		if err := rows.Scan(&r.ID, &r.Kind, &r.State, &r.Attempt, &r.LastError, &r.UpdatedAt); err != nil {
			return nil, err
		}
		out = append(out, r)
	}
	return out, rows.Err()
}

func recentAudit(ctx context.Context, db *sql.DB, limit int) ([]AuditRow, error) {
	if limit <= 0 || limit > 200 {
		limit = 25
	}
	rows, err := db.QueryContext(ctx, `
SELECT id, actor_user_id, action, ip, user_agent, created_at
FROM audit_events
ORDER BY created_at DESC
LIMIT ?
`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []AuditRow
	for rows.Next() {
		var r AuditRow
		var actor, ip, ua sql.NullString
		if err := rows.Scan(&r.ID, &actor, &r.Action, &ip, &ua, &r.CreatedAt); err != nil {
			return nil, err
		}
		if actor.Valid {
			r.ActorUserID = &actor.String
		}
		if ip.Valid {
			r.IP = &ip.String
		}
		if ua.Valid {
			r.UserAgent = &ua.String
		}
		out = append(out, r)
	}
	return out, rows.Err()
}

func testConnection(ctx context.Context, urlStr, apiKey string, policy security.OutboundPolicy) TestConnectionResponse {
	resp := TestConnectionResponse{
		URL: urlStr,
	}

	// Parse URL
	u, err := url.Parse(urlStr)
	if err != nil {
		resp.Summary = fmt.Sprintf("Invalid URL: %v", err)
		return resp
	}

	hostname := u.Hostname()
	port := u.Port()
	if port == "" {
		if u.Scheme == "https" {
			port = "443"
		} else {
			port = "80"
		}
	}

	// DNS Resolution
	dnsInfo := DNSInfo{Hostname: hostname}
	ips, err := net.LookupIP(hostname)
	if err != nil {
		dnsInfo.Error = err.Error()
	} else {
		for _, ip := range ips {
			dnsInfo.IPs = append(dnsInfo.IPs, ip.String())
		}
	}
	resp.DNSResolution = dnsInfo

	// Network Interfaces
	interfaces, err := net.Interfaces()
	if err == nil {
		for _, iface := range interfaces {
			addrs, err := iface.Addrs()
			if err != nil {
				continue
			}
			ifInfo := InterfaceInfo{Name: iface.Name}
			for _, addr := range addrs {
				if ipnet, ok := addr.(*net.IPNet); ok {
					ifInfo.IPs = append(ifInfo.IPs, ipnet.IP.String())
				}
			}
			if len(ifInfo.IPs) > 0 {
				resp.NetworkInterfaces = append(resp.NetworkInterfaces, ifInfo)
			}
		}
	}

	// TCP Connection Test (using source IP binding)
	tcpStart := time.Now()
	address := net.JoinHostPort(hostname, port)
	conn, err := netutil.DialWithSourceIP(address)
	tcpDuration := time.Since(tcpStart)
	if err != nil {
		resp.TCPTest = TCPTestResult{
			Success:  false,
			Error:    err.Error(),
			Duration: tcpDuration.String(),
		}
	} else {
		conn.Close()
		resp.TCPTest = TCPTestResult{
			Success:  true,
			Duration: tcpDuration.String(),
		}
	}

	// HTTP Test with Default Client
	httpDefaultStart := time.Now()
	defaultClient := &http.Client{Timeout: 5 * time.Second}
	testURL := fmt.Sprintf("%s/api/v1/indexer", urlStr)
	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, testURL, nil)
	if apiKey != "" {
		req.Header.Set("X-Api-Key", apiKey)
	}
	res, err := defaultClient.Do(req)
	httpDefaultDuration := time.Since(httpDefaultStart)
	if err != nil {
		resp.HTTPTestDefault = HTTPTestResult{
			Success:  false,
			Error:    err.Error(),
			Duration: httpDefaultDuration.String(),
		}
	} else {
		res.Body.Close()
		resp.HTTPTestDefault = HTTPTestResult{
			Success:   res.StatusCode == 200,
			StatusCode: res.StatusCode,
			Duration:  httpDefaultDuration.String(),
		}
	}

	// HTTP Test with Custom Transport (using source IP binding)
	httpCustomStart := time.Now()
	customClient := &http.Client{
		Timeout: 5 * time.Second,
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
	req2, _ := http.NewRequestWithContext(ctx, http.MethodGet, testURL, nil)
	if apiKey != "" {
		req2.Header.Set("X-Api-Key", apiKey)
	}
	res2, err := customClient.Do(req2)
	httpCustomDuration := time.Since(httpCustomStart)
	if err != nil {
		resp.HTTPTestCustom = HTTPTestResult{
			Success:  false,
			Error:    err.Error(),
			Duration: httpCustomDuration.String(),
		}
	} else {
		res2.Body.Close()
		resp.HTTPTestCustom = HTTPTestResult{
			Success:   res2.StatusCode == 200,
			StatusCode: res2.StatusCode,
			Duration:  httpCustomDuration.String(),
		}
	}

	// Generate summary
	if resp.TCPTest.Success && resp.HTTPTestCustom.Success {
		resp.Summary = "All tests passed - connection is working"
	} else if resp.TCPTest.Success && !resp.HTTPTestCustom.Success {
		resp.Summary = fmt.Sprintf("TCP connection works but HTTP failed: %s", resp.HTTPTestCustom.Error)
	} else if !resp.TCPTest.Success {
		resp.Summary = fmt.Sprintf("TCP connection failed: %s", resp.TCPTest.Error)
	} else {
		resp.Summary = "Connection test completed with mixed results"
	}

	return resp
}

