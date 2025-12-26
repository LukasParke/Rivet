package newznab

import (
	"context"
	"encoding/json"
	"encoding/xml"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"

	netutil "github.com/LukeHagar/Rivet/internal/network"
	"github.com/LukeHagar/Rivet/internal/security"
)

type Client struct {
	baseURL *url.URL
	apiKey  string
	policy  security.OutboundPolicy
	http    *http.Client
	name    string
	user    string
	pass    string
}

type Config struct {
	Name     string
	BaseURL  string
	APIKey   string
	Username string
	Password string
}


func New(cfg Config, policy security.OutboundPolicy) (*Client, error) {
	cfg.BaseURL = strings.TrimSpace(cfg.BaseURL)
	cfg.APIKey = strings.TrimSpace(cfg.APIKey)
	if cfg.BaseURL == "" || cfg.APIKey == "" {
		return nil, errors.New("missing_fields")
	}
	u, err := security.ValidateOutboundURL(cfg.BaseURL, policy)
	if err != nil {
		return nil, err
	}
	return &Client{
		baseURL: u,
		apiKey:  cfg.APIKey,
		policy:  policy,
		name:    strings.TrimSpace(cfg.Name),
		user:    strings.TrimSpace(cfg.Username),
		pass:    strings.TrimSpace(cfg.Password),
		http: &http.Client{
			Timeout: 10 * time.Second,
			Transport: &http.Transport{
				DialContext: func(ctx context.Context, network, addr string) (net.Conn, error) {
					return netutil.DialWithSourceIP(addr)
				},
				MaxIdleConns:          10,
				IdleConnTimeout:       90 * time.Second,
				TLSHandshakeTimeout:   5 * time.Second,
				ExpectContinueTimeout: 1 * time.Second,
			},
		},
	}, nil
}

type Item struct {
	Title       string
	GUID        string
	Link        string
	EnclosureURL string
	PubDate     string
	Attrs       map[string]string
}

// DebugInfo contains debugging information about a search request/response
type DebugInfo struct {
	URL        string
	StatusCode int
	Body       string // truncated preview (first 10KB)
	BodyLength int
	ParseError string // if parsing failed
	ItemCount  int    // number of items parsed
}

func (c *Client) Search(ctx context.Context, q string, limit int) ([]Item, error) {
	return c.SearchWithDebug(ctx, q, limit, nil)
}

// SearchWithDebug performs a search and optionally captures debug information
func (c *Client) SearchWithDebug(ctx context.Context, q string, limit int, debug *DebugInfo) ([]Item, error) {
	if limit <= 0 || limit > 100 {
		limit = 25
	}

	// Detect Prowlarr URLs upfront and use the indexer-specific endpoint directly
	if isProwlarrURL(c.baseURL.String()) {
		// For Prowlarr, find an enabled indexer and use its Torznab endpoint directly
		indexerID, err := c.findProwlarrIndexer(ctx)
		if err != nil {
			if debug != nil {
				debug.ParseError = "prowlarr_no_indexer: " + err.Error()
			}
			return nil, errors.New("prowlarr_no_indexer")
		}
		// Use the indexer-specific Torznab endpoint directly
		return c.searchProwlarrTorznab(ctx, q, limit, indexerID, debug)
	}

	// For non-Prowlarr Torznab/Newznab indexers, use the standard endpoint
	apiURL := c.baseURL.ResolveReference(&url.URL{Path: "/api"})
	qq := apiURL.Query()
	qq.Set("t", "search")
	qq.Set("q", q)
	qq.Set("apikey", c.apiKey)
	qq.Set("limit", itoa(limit))
	apiURL.RawQuery = qq.Encode()

	requestURL := apiURL.String()
	if debug != nil {
		debug.URL = requestURL
	}

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, requestURL, nil)
	// Add X-Api-Key header for better compatibility (optional for generic Torznab/Newznab)
	req.Header.Set("X-Api-Key", c.apiKey)
	if c.user != "" || c.pass != "" {
		req.SetBasicAuth(c.user, c.pass)
	}

	res, err := c.http.Do(req)
	if err != nil {
		if debug != nil {
			debug.ParseError = "connect_failed: " + err.Error()
		}
		return nil, errors.New("connect_failed")
	}
	defer res.Body.Close()

	if debug != nil {
		debug.StatusCode = res.StatusCode
	}

	if res.StatusCode != 200 {
		// Capture response body for debugging even on error
		if debug != nil {
			body, _ := io.ReadAll(io.LimitReader(res.Body, 10*1024))
			debug.Body = string(body)
			debug.BodyLength = len(body)
			debug.ParseError = "http_error_" + itoa(res.StatusCode)
		}
		return nil, errors.New("http_error")
	}

	b, _ := io.ReadAll(io.LimitReader(res.Body, 2<<20))
	if debug != nil {
		debug.BodyLength = len(b)
		// Store preview (first 10KB) for debugging
		previewLen := 10 * 1024
		if len(b) < previewLen {
			previewLen = len(b)
		}
		debug.Body = string(b[:previewLen])
		if len(b) > previewLen {
			debug.Body += "\n... (truncated)"
		}
	}

	// Check if response is JSON (shouldn't happen for non-Prowlarr, but handle gracefully)
	bodyStr := strings.TrimSpace(string(b))
	if strings.HasPrefix(bodyStr, "{") || strings.HasPrefix(bodyStr, "[") {
		// Generic JSON response - not valid for Torznab/Newznab
		if debug != nil {
			debug.ParseError = "json_response_expected_xml"
		}
		return nil, errors.New("json_response_expected_xml")
	}

	// Parse RSS items.
	var rss struct {
		Channel struct {
			Items []struct {
				Title   string `xml:"title"`
				GUID    string `xml:"guid"`
				Link    string `xml:"link"`
				PubDate string `xml:"pubDate"`
				Encl    struct {
					URL string `xml:"url,attr"`
				} `xml:"enclosure"`
				TorznabAttrs []struct {
					Name  string `xml:"name,attr"`
					Value string `xml:"value,attr"`
				} `xml:"http://torznab.com/schemas/2015/feed attr"`
				NewznabAttrs []struct {
					Name  string `xml:"name,attr"`
					Value string `xml:"value,attr"`
				} `xml:"http://www.newznab.com/DTD/2010/feeds/attributes/ attr"`
			} `xml:"item"`
		} `xml:"channel"`
	}
	if err := xml.Unmarshal(b, &rss); err != nil {
		if debug != nil {
			debug.ParseError = "parse_failed: " + err.Error()
		}
		return nil, errors.New("parse_failed")
	}

	out := make([]Item, 0, len(rss.Channel.Items))
	for _, it := range rss.Channel.Items {
		attrs := map[string]string{}
		for _, a := range it.TorznabAttrs {
			attrs[strings.ToLower(a.Name)] = a.Value
		}
		for _, a := range it.NewznabAttrs {
			attrs[strings.ToLower(a.Name)] = a.Value
		}
		out = append(out, Item{Title: strings.TrimSpace(it.Title), GUID: strings.TrimSpace(it.GUID), Link: strings.TrimSpace(it.Link), EnclosureURL: strings.TrimSpace(it.Encl.URL), PubDate: strings.TrimSpace(it.PubDate), Attrs: attrs})
	}

	if debug != nil {
		debug.ItemCount = len(out)
	}

	return out, nil
}

// findProwlarrIndexer queries Prowlarr's REST API to find the first enabled indexer.
// Returns the indexer ID, or an error if none found or API call fails.
func (c *Client) findProwlarrIndexer(ctx context.Context) (int, error) {
	indexersURL := c.baseURL.ResolveReference(&url.URL{Path: "/api/v1/indexer"})
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, indexersURL.String(), nil)
	if err != nil {
		return 0, err
	}
	req.Header.Set("X-Api-Key", c.apiKey)

	res, err := c.http.Do(req)
	if err != nil {
		// Provide more detailed error information for network diagnostics
		return 0, fmt.Errorf("prowlarr_connection_failed: %w", err)
	}
	defer res.Body.Close()

	if res.StatusCode != 200 {
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
		return 0, err
	}

	// Find the first enabled indexer
	for _, idx := range indexers {
		if idx.Enable {
			return idx.ID, nil
		}
	}

	return 0, errors.New("no_enabled_indexers")
}

// searchProwlarrTorznab performs a search using Prowlarr's indexer-specific Torznab endpoint.
func (c *Client) searchProwlarrTorznab(ctx context.Context, q string, limit int, indexerID int, debug *DebugInfo) ([]Item, error) {
	// Construct the indexer-specific Torznab endpoint
	apiURL := c.baseURL.ResolveReference(&url.URL{Path: "/api/v1/indexer/" + itoa(indexerID) + "/newznab"})
	qq := apiURL.Query()
	qq.Set("t", "search")
	qq.Set("q", q)
	qq.Set("apikey", c.apiKey)
	qq.Set("limit", itoa(limit))
	apiURL.RawQuery = qq.Encode()

	requestURL := apiURL.String()
	if debug != nil {
		debug.URL = requestURL
	}

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, requestURL, nil)
	// Use X-Api-Key header for authentication (primary method for Prowlarr)
	req.Header.Set("X-Api-Key", c.apiKey)
	if c.user != "" || c.pass != "" {
		req.SetBasicAuth(c.user, c.pass)
	}

	res, err := c.http.Do(req)
	if err != nil {
		if debug != nil {
			debug.ParseError = "connect_failed: " + err.Error()
		}
		return nil, errors.New("connect_failed")
	}
	defer res.Body.Close()

	if debug != nil {
		debug.StatusCode = res.StatusCode
	}

	if res.StatusCode != 200 {
		if debug != nil {
			body, _ := io.ReadAll(io.LimitReader(res.Body, 10*1024))
			debug.Body = string(body)
			debug.BodyLength = len(body)
			debug.ParseError = "http_error_" + itoa(res.StatusCode)
		}
		return nil, errors.New("http_error")
	}

	b, _ := io.ReadAll(io.LimitReader(res.Body, 2<<20))
	if debug != nil {
		debug.BodyLength = len(b)
		previewLen := 10 * 1024
		if len(b) < previewLen {
			previewLen = len(b)
		}
		debug.Body = string(b[:previewLen])
		if len(b) > previewLen {
			debug.Body += "\n... (truncated)"
		}
	}

	// Parse RSS items.
	var rss struct {
		Channel struct {
			Items []struct {
				Title   string `xml:"title"`
				GUID    string `xml:"guid"`
				Link    string `xml:"link"`
				PubDate string `xml:"pubDate"`
				Encl    struct {
					URL string `xml:"url,attr"`
				} `xml:"enclosure"`
				TorznabAttrs []struct {
					Name  string `xml:"name,attr"`
					Value string `xml:"value,attr"`
				} `xml:"http://torznab.com/schemas/2015/feed attr"`
				NewznabAttrs []struct {
					Name  string `xml:"name,attr"`
					Value string `xml:"value,attr"`
				} `xml:"http://www.newznab.com/DTD/2010/feeds/attributes/ attr"`
			} `xml:"item"`
		} `xml:"channel"`
	}
	if err := xml.Unmarshal(b, &rss); err != nil {
		if debug != nil {
			debug.ParseError = "parse_failed: " + err.Error()
		}
		return nil, errors.New("parse_failed")
	}

	out := make([]Item, 0, len(rss.Channel.Items))
	for _, it := range rss.Channel.Items {
		attrs := map[string]string{}
		for _, a := range it.TorznabAttrs {
			attrs[strings.ToLower(a.Name)] = a.Value
		}
		for _, a := range it.NewznabAttrs {
			attrs[strings.ToLower(a.Name)] = a.Value
		}
		out = append(out, Item{Title: strings.TrimSpace(it.Title), GUID: strings.TrimSpace(it.GUID), Link: strings.TrimSpace(it.Link), EnclosureURL: strings.TrimSpace(it.Encl.URL), PubDate: strings.TrimSpace(it.PubDate), Attrs: attrs})
	}

	if debug != nil {
		debug.ItemCount = len(out)
	}

	return out, nil
}

// isProwlarrURL detects if the URL is likely a Prowlarr instance.
func isProwlarrURL(urlStr string) bool {
	urlStr = strings.ToLower(urlStr)
	return strings.Contains(urlStr, "prowlarr") || 
		strings.Contains(urlStr, ":9696") ||
		strings.Contains(urlStr, "/prowlarr")
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	buf := make([]byte, 0, 16)
	for n > 0 {
		d := n % 10
		buf = append(buf, byte('0'+d))
		n /= 10
	}
	for i, j := 0, len(buf)-1; i < j; i, j = i+1, j-1 {
		buf[i], buf[j] = buf[j], buf[i]
	}
	return string(buf)
}
