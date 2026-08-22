//! Prometheus exposition endpoint (`GET /metrics`).

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// Content type mandated by the Prometheus text exposition format 0.0.4.
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// GET /metrics — renders the registry as Prometheus text format.
pub async fn metrics_handler(State(state): State<AppState>) -> Response {
    let body = state.metrics.render();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn test_state(metrics: crate::metrics::Metrics) -> AppState {
        AppState {
            db: crate::storage::open("sqlite::memory:").await.unwrap(),
            secret_box: Arc::new(crate::security::secrets::SecretBox::new(None).unwrap()),
            policy: crate::security::OutboundPolicy::dev(),
            hub: Arc::new(Hub::new()),
            metrics: Arc::new(metrics),
            config: Arc::new(crate::config::Config {
                env: "test".into(),
                http_addr: "127.0.0.1:0".into(),
                database_url: String::new(),
                dry_run: false,
                dry_run_fixture_path: None,
                ffprobe_path: None,
                secret_key_base64: None,
                session_cookie_name: "rivet_session".into(),
                session_ttl: Duration::from_secs(3600),
                allow_internal_urls: true,
            }),
        }
    }

    #[tokio::test]
    async fn renders_prometheus_counters() {
        let m = crate::metrics::Metrics::default();
        m.inc_jobs_enqueued();
        m.inc_jobs_enqueued();
        m.inc_jobs_completed();
        m.set_queue_depth("queued", 3);

        let app = Router::new()
            .route("/metrics", get(metrics_handler))
            .with_state(test_state(m).await);

        let res = app
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            PROMETHEUS_CONTENT_TYPE
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("rivet_jobs_enqueued_total 2"), "body: {text}");
        assert!(
            text.contains("rivet_jobs_completed_total 1"),
            "body: {text}"
        );
        assert!(
            text.contains("rivet_queue_depth{state=\"queued\"} 3"),
            "body: {text}"
        );
    }
}
