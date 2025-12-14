package jobs

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"log/slog"
	"time"

	"github.com/google/uuid"

	"github.com/LukeHagar/Rivet/internal/jobs/queue"
	"github.com/LukeHagar/Rivet/internal/security"
	"github.com/LukeHagar/Rivet/internal/jobs/handlers"
)

type WorkerDeps struct {
	DB        *sql.DB
	SecretBox *security.SecretBox
	Policy    security.OutboundPolicy
	Logger    *slog.Logger
}

// StartWorker starts a simple in-process worker loop.
// Scale mode will move this into cmd/worker with distributed leases.
func StartWorker(ctx context.Context, deps WorkerDeps) {
	owner := uuid.NewString()
	log := deps.Logger
	if log == nil {
		log = slog.Default()
	}

	go func() {
		t := time.NewTicker(750 * time.Millisecond)
		defer t.Stop()

		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				job, err := queue.ClaimOne(ctx, deps.DB, queue.ClaimOpts{LeaseOwner: owner, LeaseDuration: 30 * time.Second})
				if err != nil {
					if errors.Is(err, queue.ErrNoJob) {
						continue
					}
					log.Error("claim job", slog.Any("err", err))
					continue
				}

				hErr := handleJob(ctx, deps, job)
				if hErr == nil {
					_ = queue.Complete(ctx, deps.DB, job.ID, time.Now().UTC())
					continue
				}

				attempt := job.Attempt + 1
				errStr := hErr.Error()
				if attempt >= 6 {
					_ = queue.FailFinal(ctx, deps.DB, job.ID, attempt, errStr, time.Now().UTC())
				} else {
					_ = queue.FailAndRetry(ctx, deps.DB, job.ID, attempt, errStr, time.Now().UTC())
				}
			}
		}
	}()
}

func handleJob(ctx context.Context, deps WorkerDeps, job queue.Job) error {
	switch job.Kind {
	case "search_run":
		var p struct {
			RunID    string `json:"runId"`
			TargetID string `json:"targetId"`
		}
		if job.PayloadJSON == nil {
			return errors.New("missing_payload")
		}
		if err := json.Unmarshal([]byte(*job.PayloadJSON), &p); err != nil {
			return err
		}
		if p.RunID == "" || p.TargetID == "" {
			return errors.New("invalid_payload")
		}

		return handlers.ExecuteSearchRun(ctx, handlers.SearchRunDeps{DB: deps.DB, SecretBox: deps.SecretBox, Policy: deps.Policy}, p.RunID, p.TargetID)
	default:
		return errors.New("unknown_job_kind")
	}
}
