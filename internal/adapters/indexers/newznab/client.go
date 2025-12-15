package newznab

import (
	"context"
	"encoding/xml"
	"errors"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

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
		http:    &http.Client{Timeout: 10 * time.Second},
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

func (c *Client) Search(ctx context.Context, q string, limit int) ([]Item, error) {
	if limit <= 0 || limit > 100 {
		limit = 25
	}
	apiURL := c.baseURL.ResolveReference(&url.URL{Path: "/api"})
	qq := apiURL.Query()
	qq.Set("t", "search")
	qq.Set("q", q)
	qq.Set("apikey", c.apiKey)
	qq.Set("limit", itoa(limit))
	apiURL.RawQuery = qq.Encode()

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, apiURL.String(), nil)
	if c.user != "" || c.pass != "" {
		req.SetBasicAuth(c.user, c.pass)
	}

	res, err := c.http.Do(req)
	if err != nil {
		return nil, errors.New("connect_failed")
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		return nil, errors.New("http_error")
	}

	b, _ := io.ReadAll(io.LimitReader(res.Body, 2<<20))

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
	return out, nil
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
