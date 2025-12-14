package security

import (
	"crypto/subtle"
	"errors"
	"strconv"
	"strings"
)

func subtleConstantTimeEq(a, b []byte) bool {
	if len(a) != len(b) {
		return false
	}
	return subtle.ConstantTimeCompare(a, b) == 1
}

func split(s string, sep byte) []string {
	// strings.Split is fine here, but we want a tiny helper that matches the parse needs.
	return strings.Split(s, string(sep))
}

func cutPrefix(s, prefix string) (string, bool) {
	if !strings.HasPrefix(s, prefix) {
		return "", false
	}
	return strings.TrimPrefix(s, prefix), true
}

func parseUint(s string) (uint64, error) {
	u, err := strconv.ParseUint(strings.TrimSpace(s), 10, 64)
	if err != nil {
		return 0, errors.New("parse uint")
	}
	return u, nil
}

func itoa(n uint32) string {
	return strconv.FormatUint(uint64(n), 10)
}
