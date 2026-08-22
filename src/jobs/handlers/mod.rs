//! Job handlers: one module per job kind, dispatched by `job.kind`.
//!
//! Contract (review fixes applied across all handlers):
//! - Every continuation enqueue propagates failures — never `let _ = enqueue`.
//! - Handlers return `Err(String)`; the worker turns that into a retry with
//!   backoff and, at [`crate::storage::jobs::MAX_ATTEMPTS`], a final failure.
//! - Panics are isolated by the worker (`tokio::spawn` + `JoinError`).

pub mod acquisition_enqueue;
pub mod acquisition_import;
pub mod acquisition_monitor;
pub mod acquisition_recover;
pub mod acquisition_rescout;
pub mod acquisition_verify;
pub mod enrich_series;
pub mod search_common;
pub mod search_run;

pub use search_common::{
    blacklist_set, extract_btih, load_indexers, load_policies, load_setup_paths, load_target_query,
    load_torrent_client, tried_external_ids, SetupPaths,
};

use crate::state::AppState;
use crate::storage::jobs::Job;

/// Dispatches a claimed job to its handler. Unknown kinds fail the job.
pub async fn dispatch(state: &AppState, job: &Job) -> Result<(), String> {
    tracing::debug!(job_id = %job.id, kind = %job.kind, attempt = job.attempt, "job started");
    match job.kind.as_str() {
        "search_run" => search_run::handle(state, job).await,
        "acquisition_enqueue" => acquisition_enqueue::handle(state, job).await,
        "acquisition_monitor" => acquisition_monitor::handle(state, job).await,
        "acquisition_verify" => acquisition_verify::handle(state, job).await,
        "acquisition_import" => acquisition_import::handle(state, job).await,
        "acquisition_recover" => acquisition_recover::handle(state, job).await,
        "acquisition_rescout" => acquisition_rescout::handle(state, job).await,
        "enrich_series" => enrich_series::handle(state, job).await,
        #[cfg(test)]
        "test_panic" => panic!("intentional test panic"),
        _ => Err("unknown_job_kind".to_string()),
    }
}

/// Publishes a WebSocket event; hub publication never fails a job.
pub fn publish(state: &AppState, event_type: &str, payload: serde_json::Value) {
    state.hub.publish(event_type, payload);
    state.metrics.inc_ws_events();
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Shared test scaffolding: an AppState over a tempdir database.

    use std::sync::Arc;

    use crate::state::AppState;

    pub async fn test_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::storage::open(&format!("file://{}", dir.path().join("test.db").display()))
            .await
            .unwrap();
        crate::storage::migrations::apply(&db).await.unwrap();
        let secret_box = Arc::new(crate::security::secrets::SecretBox::new(None).unwrap());
        let state = AppState {
            db,
            secret_box,
            policy: crate::security::OutboundPolicy {
                allow_internal: false,
            },
            hub: Arc::new(crate::ws::hub::Hub::new()),
            metrics: Arc::new(crate::metrics::Metrics::default()),
            config: Arc::new(crate::config::Config {
                env: "test".into(),
                http_addr: "127.0.0.1:0".into(),
                database_url: String::new(),
                dry_run: true,
                dry_run_fixture_path: None,
                ffprobe_path: None,
                secret_key_base64: None,
                session_cookie_name: "rivet_session".into(),
                session_ttl: std::time::Duration::from_secs(3600),
                allow_internal_urls: false,
            }),
        };
        (dir, state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::handlers::testutil;

    #[tokio::test]
    async fn dispatch_unknown_kind_fails() {
        let (_dir, state) = testutil::test_state().await;
        let job = crate::storage::jobs::enqueue::<serde_json::Value>(&state.db, "nope", None, None)
            .await
            .unwrap();
        let err = dispatch(&state, &job).await.unwrap_err();
        assert_eq!(err, "unknown_job_kind");
    }

    #[tokio::test]
    async fn dispatch_reaches_each_handler_module() {
        // A kind whose handler rejects an empty payload proves dispatch wiring
        // without needing external clients.
        let (_dir, state) = testutil::test_state().await;
        for kind in [
            "search_run",
            "acquisition_enqueue",
            "acquisition_monitor",
            "acquisition_verify",
            "acquisition_import",
            "acquisition_recover",
            "acquisition_rescout",
        ] {
            let job =
                crate::storage::jobs::enqueue::<serde_json::Value>(&state.db, kind, None, None)
                    .await
                    .unwrap();
            assert_eq!(
                dispatch(&state, &job).await.unwrap_err(),
                "missing_payload",
                "{kind}"
            );
        }
    }
}
