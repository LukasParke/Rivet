package observability

import (
	"fmt"
	"sync/atomic"
)

type Metrics struct {
	JobsEnqueued       atomic.Int64
	JobsClaimed        atomic.Int64
	JobsCompleted      atomic.Int64
	JobsRetryScheduled atomic.Int64
	JobsFailed         atomic.Int64

	SearchRunsStarted   atomic.Int64
	SearchRunsCompleted atomic.Int64
	SearchRunsFailed    atomic.Int64
	CandidatesRecorded  atomic.Int64
	DecisionsRecorded   atomic.Int64
}

func (m *Metrics) RenderText() string {
	if m == nil {
		return ""
	}
	// Prometheus-style exposition (minimal).
	return fmt.Sprintf(
		"# TYPE rivet_jobs_enqueued_total counter\nrivet_jobs_enqueued_total %d\n"+
			"# TYPE rivet_jobs_claimed_total counter\nrivet_jobs_claimed_total %d\n"+
			"# TYPE rivet_jobs_completed_total counter\nrivet_jobs_completed_total %d\n"+
			"# TYPE rivet_jobs_retry_scheduled_total counter\nrivet_jobs_retry_scheduled_total %d\n"+
			"# TYPE rivet_jobs_failed_total counter\nrivet_jobs_failed_total %d\n"+
			"# TYPE rivet_search_runs_started_total counter\nrivet_search_runs_started_total %d\n"+
			"# TYPE rivet_search_runs_completed_total counter\nrivet_search_runs_completed_total %d\n"+
			"# TYPE rivet_search_runs_failed_total counter\nrivet_search_runs_failed_total %d\n"+
			"# TYPE rivet_candidates_recorded_total counter\nrivet_candidates_recorded_total %d\n"+
			"# TYPE rivet_decisions_recorded_total counter\nrivet_decisions_recorded_total %d\n",
		m.JobsEnqueued.Load(),
		m.JobsClaimed.Load(),
		m.JobsCompleted.Load(),
		m.JobsRetryScheduled.Load(),
		m.JobsFailed.Load(),
		m.SearchRunsStarted.Load(),
		m.SearchRunsCompleted.Load(),
		m.SearchRunsFailed.Load(),
		m.CandidatesRecorded.Load(),
		m.DecisionsRecorded.Load(),
	)
}
