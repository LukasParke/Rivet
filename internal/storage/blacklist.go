package storage

import (
	"context"
	"database/sql"
	"errors"
	"strings"
	"time"

	"github.com/google/uuid"
)

type BlacklistEntry struct {
	ID              string
	Scope           string
	TargetID        *string
	MatchType       string
	MatchValue      string
	Reason          *string
	CreatedByUserID *string
	CreatedAt       time.Time
}

func InsertBlacklistEntry(ctx context.Context, db *sql.DB, e BlacklistEntry) (BlacklistEntry, error) {
	now := time.Now().UTC()
	e.ID = strings.TrimSpace(e.ID)
	if e.ID == "" {
		e.ID = uuid.NewString()
	}
	e.Scope = strings.ToLower(strings.TrimSpace(e.Scope))
	if e.Scope == "" {
		e.Scope = "target"
	}
	if e.Scope != "global" && e.Scope != "target" {
		return BlacklistEntry{}, errors.New("invalid_scope")
	}
	if e.Scope == "target" {
		if e.TargetID == nil || strings.TrimSpace(*e.TargetID) == "" {
			return BlacklistEntry{}, errors.New("target_id_required")
		}
		tid := strings.TrimSpace(*e.TargetID)
		e.TargetID = &tid
	} else {
		e.TargetID = nil
	}

	e.MatchType = strings.ToLower(strings.TrimSpace(e.MatchType))
	if e.MatchType == "" {
		return BlacklistEntry{}, errors.New("match_type_required")
	}
	switch e.MatchType {
	case "hash", "url", "title":
	default:
		return BlacklistEntry{}, errors.New("invalid_match_type")
	}

	e.MatchValue = normalizeMatchValue(e.MatchValue)
	if e.MatchValue == "" {
		return BlacklistEntry{}, errors.New("match_value_required")
	}

	if e.Reason != nil {
		s := strings.TrimSpace(*e.Reason)
		if s == "" {
			e.Reason = nil
		} else {
			e.Reason = &s
		}
	}
	if e.CreatedByUserID != nil {
		s := strings.TrimSpace(*e.CreatedByUserID)
		if s == "" {
			e.CreatedByUserID = nil
		} else {
			e.CreatedByUserID = &s
		}
	}

	_, err := db.ExecContext(ctx, `
INSERT INTO blacklist_entries (id, scope, target_id, match_type, match_value, reason, created_by_user_id, created_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?)
`, e.ID, e.Scope, e.TargetID, e.MatchType, e.MatchValue, e.Reason, e.CreatedByUserID, now.Format(time.RFC3339Nano))
	if err != nil {
		return BlacklistEntry{}, err
	}
	e.CreatedAt = now
	return e, nil
}

// LoadBlacklistSet returns a set of blacklisted values by match_type for the provided target.
// It includes both global entries and target-scoped entries.
func LoadBlacklistSet(ctx context.Context, db *sql.DB, targetID string) (map[string]map[string]bool, error) {
	targetID = strings.TrimSpace(targetID)

	q := `
SELECT match_type, match_value
FROM blacklist_entries
WHERE scope = 'global'
`
	args := []any{}
	if targetID != "" {
		q += " OR (scope = 'target' AND target_id = ?)\n"
		args = append(args, targetID)
	}

	rows, err := db.QueryContext(ctx, q, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	out := map[string]map[string]bool{}
	for rows.Next() {
		var t, v string
		if err := rows.Scan(&t, &v); err != nil {
			return nil, err
		}
		t = strings.ToLower(strings.TrimSpace(t))
		v = normalizeMatchValue(v)
		if t == "" || v == "" {
			continue
		}
		m, ok := out[t]
		if !ok {
			m = map[string]bool{}
			out[t] = m
		}
		m[v] = true
	}
	return out, rows.Err()
}

func normalizeMatchValue(s string) string {
	return strings.ToLower(strings.TrimSpace(s))
}


