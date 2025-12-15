package security

import (
	"context"
	"errors"
	"net"
	"net/url"
	"strings"
)

type OutboundPolicy struct {
	AllowInternal bool
}

func ValidateOutboundURL(raw string, p OutboundPolicy) (*url.URL, error) {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return nil, errors.New("url required")
	}
	u, err := url.Parse(raw)
	if err != nil {
		return nil, errors.New("invalid url")
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return nil, errors.New("unsupported scheme")
	}
	if u.Hostname() == "" {
		return nil, errors.New("host required")
	}

	// Resolve and block internal ranges unless explicitly allowed.
	if !p.AllowInternal {
		ips, err := net.DefaultResolver.LookupIPAddr(context.Background(), u.Hostname())
		if err != nil {
			return nil, errors.New("dns lookup failed")
		}
		for _, ip := range ips {
			if isInternalIP(ip.IP) {
				return nil, errors.New("internal address not allowed")
			}
		}
	}

	return u, nil
}

func isInternalIP(ip net.IP) bool {
	if ip == nil {
		return true
	}
	if ip.IsLoopback() || ip.IsLinkLocalMulticast() || ip.IsLinkLocalUnicast() {
		return true
	}
	if ip.IsPrivate() {
		return true
	}
	// Unique local IPv6 (fc00::/7) is covered by IsPrivate() in recent Go,
	// but keep explicit checks minimal.
	return false
}
