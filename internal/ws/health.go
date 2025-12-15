package ws

import (
	"context"
	"database/sql"
	"time"

	"github.com/LukeHagar/Rivet/internal/observability"
)

type HealthPublisherDeps struct {
	DB      *sql.DB
	Hub     *Hub
	Metrics *observability.Metrics
}

func StartHealthPublisher(ctx context.Context, deps HealthPublisherDeps, every time.Duration) {
	if deps.Hub == nil || deps.DB == nil {
		return
	}
	if every <= 0 {
		every = 10 * time.Second
	}

	go func() {
		t := time.NewTicker(every)
		defer t.Stop()

		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				pingCtx, cancel := context.WithTimeout(ctx, 2*time.Second)
				err := deps.DB.PingContext(pingCtx)
				cancel()

				payload := map[string]any{
					"ok": err == nil,
				}
				if err != nil {
					payload["dbError"] = err.Error()
				}

				if deps.Metrics != nil {
					payload["metrics"] = map[string]int64{
						"jobs_enqueued_total": deps.Metrics.JobsEnqueued.Load(),
						"jobs_claimed_total": deps.Metrics.JobsClaimed.Load(),
						"jobs_completed_total": deps.Metrics.JobsCompleted.Load(),
						"jobs_failed_total": deps.Metrics.JobsFailed.Load(),
						"search_runs_started_total": deps.Metrics.SearchRunsStarted.Load(),
						"search_runs_completed_total": deps.Metrics.SearchRunsCompleted.Load(),
					}
				}

				deps.Hub.Publish("health.snapshot", payload)
			}
		}
	}()
}
