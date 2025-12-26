package verify

import (
	"context"
	"encoding/json"
	"errors"
	"os/exec"
	"strconv"
	"strings"
	"time"
)

type FFProbeSummary struct {
	DurationSeconds float64 `json:"durationSeconds"`
	Width           int     `json:"width,omitempty"`
	Height          int     `json:"height,omitempty"`
}

// ProbeFFProbe runs ffprobe (if available) and returns a small summary.
// ok=false means ffprobe is not available on this system.
func ProbeFFProbe(ctx context.Context, ffprobePath string, mediaPath string) (FFProbeSummary, bool, error) {
	mediaPath = strings.TrimSpace(mediaPath)
	if mediaPath == "" {
		return FFProbeSummary{}, true, errors.New("path_required")
	}

	ffprobePath = strings.TrimSpace(ffprobePath)
	if ffprobePath == "" {
		p, err := exec.LookPath("ffprobe")
		if err != nil {
			return FFProbeSummary{}, false, nil
		}
		ffprobePath = p
	}

	// Be conservative: don't let ffprobe hang.
	if ctx == nil {
		ctx = context.Background()
	}
	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()

	// duration from format, plus width/height from first video stream (if any).
	args := []string{
		"-v", "error",
		"-select_streams", "v:0",
		"-show_entries", "format=duration:stream=width,height",
		"-of", "json",
		mediaPath,
	}
	out, err := exec.CommandContext(ctx, ffprobePath, args...).Output()
	if err != nil {
		return FFProbeSummary{}, true, errors.New("ffprobe_failed")
	}

	var parsed struct {
		Format struct {
			Duration string `json:"duration"`
		} `json:"format"`
		Streams []struct {
			Width  int `json:"width"`
			Height int `json:"height"`
		} `json:"streams"`
	}
	if err := json.Unmarshal(out, &parsed); err != nil {
		return FFProbeSummary{}, true, errors.New("ffprobe_parse_failed")
	}

	dur := 0.0
	if strings.TrimSpace(parsed.Format.Duration) != "" {
		if f, err := strconv.ParseFloat(strings.TrimSpace(parsed.Format.Duration), 64); err == nil {
			dur = f
		}
	}
	s := FFProbeSummary{DurationSeconds: dur}
	if len(parsed.Streams) > 0 {
		s.Width = parsed.Streams[0].Width
		s.Height = parsed.Streams[0].Height
	}
	return s, true, nil
}


