//! Minimal Prometheus text-format metrics, mirroring the original counters.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Default)]
pub struct Metrics {
    pub jobs_enqueued_total: AtomicU64,
    pub jobs_claimed_total: AtomicU64,
    pub jobs_completed_total: AtomicU64,
    pub jobs_retry_scheduled_total: AtomicU64,
    pub ws_events_published_total: AtomicU64,
    /// Gauge: current queue depth by state ("queued", "running", "failed").
    pub queue_depth: Mutex<std::collections::HashMap<String, i64>>,
}

impl Metrics {
    pub fn inc_jobs_enqueued(&self) {
        self.jobs_enqueued_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_jobs_claimed(&self) {
        self.jobs_claimed_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_jobs_completed(&self) {
        self.jobs_completed_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_jobs_retried(&self) {
        self.jobs_retry_scheduled_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_ws_events(&self) {
        self.ws_events_published_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_queue_depth(&self, state: &str, v: i64) {
        self.queue_depth
            .lock()
            .expect("queue depth mutex")
            .insert(state.to_string(), v);
    }

    /// Renders the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        let counter = |out: &mut String, name: &str, help: &str, v: u64| {
            out.push_str("# TYPE ");
            out.push_str(name);
            out.push_str(" counter\n");
            out.push_str(help);
            out.push_str(name);
            out.push(' ');
            out.push_str(&v.to_string());
            out.push('\n');
        };
        counter(
            &mut out,
            "rivet_jobs_enqueued_total",
            "# HELP rivet_jobs_enqueued_total Jobs enqueued.\n",
            self.jobs_enqueued_total.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "rivet_jobs_claimed_total",
            "# HELP rivet_jobs_claimed_total Jobs claimed by workers.\n",
            self.jobs_claimed_total.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "rivet_jobs_completed_total",
            "# HELP rivet_jobs_completed_total Jobs completed successfully.\n",
            self.jobs_completed_total.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "rivet_jobs_retry_scheduled_total",
            "# HELP rivet_jobs_retry_scheduled_total Jobs scheduled for retry.\n",
            self.jobs_retry_scheduled_total.load(Ordering::Relaxed),
        );
        if let Ok(map) = self.queue_depth.lock() {
            out.push_str("# TYPE rivet_queue_depth gauge\n");
            for (state, v) in map.iter() {
                out.push_str("rivet_queue_depth{state=\"");
                out.push_str(state);
                out.push_str("\"} ");
                out.push_str(&v.to_string());
                out.push('\n');
            }
        }
        out
    }
}
