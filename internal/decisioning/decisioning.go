package decisioning

import (
	"encoding/json"
	"strings"
)

type Policies struct {
	Language        string   `json:"language"`
	BannedTags      []string `json:"bannedTags"`
	QualityProfiles []struct {
		Name             string   `json:"name"`
		AllowedQualities []string `json:"allowedQualities"`
		UpgradeAllowed   bool     `json:"upgradeAllowed"`
	} `json:"qualityProfiles"`
}

type Normalized struct {
	Title   string   `json:"title"`
	Quality string   `json:"quality"`
	Tags    []string `json:"tags"`
}

type DecisionResult struct {
	Accepted bool     `json:"accepted"`
	Score    int      `json:"score"`
	Reasons  []string `json:"reasons"`
}

func ParsePolicies(jsonStr string) (Policies, error) {
	var p Policies
	err := json.Unmarshal([]byte(jsonStr), &p)
	return p, err
}

func NormalizeTitle(title string) Normalized {
	t := strings.TrimSpace(title)
	lt := strings.ToLower(t)

	quality := ""
	for _, q := range []string{"2160p", "1080p", "720p", "480p"} {
		if strings.Contains(lt, q) {
			quality = q
			break
		}
	}

	seps := func(r rune) bool {
		return r == '.' || r == '-' || r == '_' || r == ' ' || r == '[' || r == ']' || r == '(' || r == ')'
	}
	parts := strings.FieldsFunc(lt, seps)
	seen := map[string]bool{}
	var tags []string
	for _, p := range parts {
		p = strings.TrimSpace(p)
		if p == "" || len(p) > 32 {
			continue
		}
		if !seen[p] {
			seen[p] = true
			tags = append(tags, p)
		}
	}

	return Normalized{Title: t, Quality: quality, Tags: tags}
}

func Decide(n Normalized, policies Policies) DecisionResult {
	allowed := map[string]bool{}
	if len(policies.QualityProfiles) > 0 {
		for _, q := range policies.QualityProfiles[0].AllowedQualities {
			allowed[strings.ToLower(strings.TrimSpace(q))] = true
		}
	}

	var reasons []string
	lt := strings.ToLower(n.Title)
	for _, bt := range policies.BannedTags {
		bt = strings.ToLower(strings.TrimSpace(bt))
		if bt == "" {
			continue
		}
		if strings.Contains(lt, bt) {
			reasons = append(reasons, "banned_tag:"+bt)
			return DecisionResult{Accepted: false, Score: 0, Reasons: reasons}
		}
	}

	if n.Quality != "" && len(allowed) > 0 {
		if !allowed[strings.ToLower(n.Quality)] {
			reasons = append(reasons, "quality_not_allowed:"+n.Quality)
			return DecisionResult{Accepted: false, Score: 0, Reasons: reasons}
		}
		reasons = append(reasons, "quality_allowed:"+n.Quality)
	}

	score := 10
	switch n.Quality {
	case "2160p":
		score = 40
	case "1080p":
		score = 30
	case "720p":
		score = 20
	case "480p":
		score = 15
	default:
		score = 10
	}
	if len(reasons) == 0 {
		reasons = append(reasons, "no_hard_rejects")
	}

	return DecisionResult{Accepted: true, Score: score, Reasons: reasons}
}
