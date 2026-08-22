//! Rivet server entry point.

use std::sync::Arc;

use rivet::config::Config;
use rivet::metrics::Metrics;
use rivet::security::secrets::SecretBox;
use rivet::security::OutboundPolicy;
use rivet::state::{AppState, Role};
use rivet::ws::hub::Hub;
use rivet::{httpapi, jobs, logging, storage};

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async_main());
}

async fn async_main() {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };
    logging::init(&cfg.env);
    let cfg = Arc::new(cfg);

    let db = match storage::open(&cfg.database_url).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "database open failed");
            std::process::exit(1);
        }
    };
    if let Err(e) = storage::migrations::apply(&db).await {
        tracing::error!(error = %e, "migrations failed");
        std::process::exit(1);
    }

    // One SecretBox shared by API and worker (review fix: independent dev keys
    // made wizard-saved credentials undecryptable by jobs).
    let secret_box = match SecretBox::new(cfg.secret_key_base64.as_deref()) {
        Ok(sb) => Arc::new(sb),
        Err(e) => {
            tracing::error!(error = %e, "secret key invalid");
            std::process::exit(1);
        }
    };

    let hub = Arc::new(Hub::new());
    let metrics = Arc::new(Metrics::default());
    let policy = if cfg.allow_internal_urls {
        OutboundPolicy::dev()
    } else {
        OutboundPolicy::production()
    };

    let gate = rivet::httpapi::middleware::setup_gate::init_shared(
        db.clone(),
        std::time::Duration::from_secs(2),
    )
    .await;
    gate.start_refresher().await;

    let state = AppState {
        db: db.clone(),
        secret_box,
        policy,
        hub: hub.clone(),
        metrics: metrics.clone(),
        config: cfg.clone(),
    };

    // Durable worker + reconciler/retention loops.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let worker_handle = jobs::worker::spawn(state.clone(), shutdown_rx.clone());
    let reconciler_handle = jobs::reconciler::spawn(state.clone(), shutdown_rx);

    // Health snapshot publisher over WS.
    {
        let (state, hub) = (state.clone(), hub.clone());
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                ticker.tick().await;
                if let Ok(snapshot) = health_snapshot(&state).await {
                    hub.publish("health.snapshot", snapshot);
                }
            }
        });
    }

    let app = httpapi::build_router(state.clone(), gate);
    let addr = cfg.http_addr.clone();
    // Listener with per-connection socket options: accepted connections
    // inherit TCP_NODELAY (no Nagle latency) and SO_KEEPALIVE (dead-peer
    // reaping) from the listening socket on Linux/BSD.
    let listener = match bind_listener(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "bind failed");
            std::process::exit(1);
        }
    };
    tracing::info!(%addr, env = %cfg.env, dry_run = cfg.dry_run, "rivet listening");

    // Server-level timeouts live in httpapi::build_router (30s request
    // timeout + 1 MiB body limit). Remaining gap: hyper has no inter-read
    // deadline, so a slowloris client trickling bytes is only bounded by the
    // 30s total request timeout. nginx fronts production and must keep its
    // client_header_timeout/client_body_timeout set; see the module comment
    // in httpapi/mod.rs for the direct-exposure mitigation.
    let axum_app = app.into_make_service();

    let shutdown = async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = shutdown_tx.send(true);
    };
    if let Err(e) = axum::serve(listener, axum_app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        tracing::error!(error = %e, "server error");
    }

    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), worker_handle).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), reconciler_handle).await;
}

/// Binds `addr` with TCP_NODELAY and SO_KEEPALIVE set on the listening socket
/// so every accepted connection inherits them. Keeps the semantics of
/// `TcpListener::bind` (first resolved address, same default backlog).
async fn bind_listener(
    addr: &str,
) -> std::io::Result<tokio::net::TcpListener> {
    use std::net::ToSocketAddrs;
    let sa: std::net::SocketAddr = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "no address resolved")
        })?;
    let sock = match sa {
        std::net::SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        std::net::SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
    };
    sock.set_nodelay(true)?;
    sock.set_keepalive(true)?;
    sock.bind(sa)?;
    sock.listen(1024)
}

/// Queue depths + user count for the health channel payload.
async fn health_snapshot(state: &AppState) -> Result<serde_json::Value, sqlx::Error> {
    let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE state = 'queued'")
        .fetch_one(&state.db)
        .await?;
    let running: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE state = 'queued' AND lease_owner IS NOT NULL",
    )
    .fetch_one(&state.db)
    .await?;
    let failed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE state = 'failed'")
        .fetch_one(&state.db)
        .await?;
    let users: i64 = storage::users::count_users(&state.db).await?;
    Ok(serde_json::json!({
        "jobs": {"queued": queued, "running": running, "failed": failed},
        "users": users,
        "dryRun": state.config.dry_run,
    }))
}

// Keep Role referenced for type stability of AppState wiring in tests.
#[allow(dead_code)]
fn _role_assert() {
    let _ = Role::Admin.rank();
}
