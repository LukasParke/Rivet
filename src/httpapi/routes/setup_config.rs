//! `GET /setup/config`: admin-only dump of the wizard configuration with
//! secrets decrypted for redisplay in the UI.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::httpapi::middleware::auth::Admin;
use crate::state::AppState;

#[derive(Debug, Serialize)]
struct DecryptedConfigResponse {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    paths: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    clients: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    indexers: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    policies: BTreeMap<String, Value>,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new().route("/setup/config", axum::routing::get(config))
}

/// Loads a settings key and parses it as a JSON object.
async fn load_object(state: &AppState, key: &str) -> BTreeMap<String, Value> {
    match crate::storage::settings::get(&state.db, key).await {
        Ok(Some(raw)) => serde_json::from_str::<BTreeMap<String, Value>>(&raw).unwrap_or_default(),
        _ => BTreeMap::new(),
    }
}

/// Decrypts `encKey` inside `obj` (when present and decryptable) into `plainKey`.
fn decrypt_into(
    state: &AppState,
    obj: &mut BTreeMap<String, Value>,
    plain_key: &str,
    enc_key: &str,
) {
    let Some(enc) = obj
        .get(enc_key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    if let Ok(plain) = state.secret_box.decrypt_string(enc) {
        if let Ok(text) = String::from_utf8(plain) {
            obj.insert(plain_key.to_string(), Value::String(text));
        }
    }
}

async fn config(_user: Admin, State(state): State<AppState>) -> Result<Response, AppError> {
    // Paths and policies are stored as-is.
    let paths = load_object(&state, "setup.paths").await;
    let policies = load_object(&state, "setup.policies").await;

    // Clients: decrypt passwords / API keys.
    let mut clients = BTreeMap::new();
    for (client, plain_key) in [
        ("qbittorrent", "password"),
        ("nzbget", "password"),
        ("sabnzbd", "apiKey"),
    ] {
        let Some(raw) = load_object(&state, "setup.clients")
            .await
            .get(client)
            .cloned()
        else {
            continue;
        };
        let mut obj: BTreeMap<String, Value> = serde_json::from_value(raw).unwrap_or_default();
        decrypt_into(&state, &mut obj, plain_key, &format!("{plain_key}Enc"));
        clients.insert(
            client.to_string(),
            serde_json::to_value(obj).unwrap_or(json!({})),
        );
    }

    // Indexers: decrypt apiKey + password per entry (torznab + newznab lists).
    let mut indexers = BTreeMap::new();
    for kind in ["torznab", "newznab"] {
        let Some(list) = load_object(&state, "setup.indexers")
            .await
            .get(kind)
            .cloned()
        else {
            continue;
        };
        let Some(entries) = list.as_array() else {
            continue;
        };
        let mut out: Vec<Value> = Vec::with_capacity(entries.len());
        for entry in entries {
            let mut obj: BTreeMap<String, Value> =
                serde_json::from_value(entry.clone()).unwrap_or_default();
            decrypt_into(&state, &mut obj, "apiKey", "apiKeyEnc");
            decrypt_into(&state, &mut obj, "password", "passwordEnc");
            out.push(serde_json::to_value(obj).unwrap_or(json!({})));
        }
        indexers.insert(kind.to_string(), Value::Array(out));
    }

    // Metadata: decrypt API keys.
    let mut metadata = BTreeMap::new();
    for provider in ["tmdb", "tvdb"] {
        let Some(raw) = load_object(&state, "setup.metadata")
            .await
            .get(provider)
            .cloned()
        else {
            continue;
        };
        let mut obj: BTreeMap<String, Value> = serde_json::from_value(raw).unwrap_or_default();
        decrypt_into(&state, &mut obj, "apiKey", "apiKeyEnc");
        metadata.insert(
            provider.to_string(),
            serde_json::to_value(obj).unwrap_or(json!({})),
        );
    }

    Ok(axum::Json(DecryptedConfigResponse {
        paths,
        clients,
        indexers,
        metadata,
        policies,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::httpapi::middleware::auth::{csrf, load_session};
    use crate::metrics::Metrics;
    use crate::security::secrets::SecretBox;
    use crate::security::OutboundPolicy;
    use crate::ws::hub::Hub;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::middleware;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn app_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::storage::open(&format!(
            "file://{}",
            dir.path().join("config_test.db").display()
        ))
        .await
        .unwrap();
        crate::storage::migrations::apply(&db).await.unwrap();
        let config = Arc::new(crate::config::Config {
            env: "test".into(),
            http_addr: String::new(),
            database_url: String::new(),
            dry_run: false,
            dry_run_fixture_path: None,
            ffprobe_path: None,
            secret_key_base64: None,
            session_cookie_name: "rivet_session".into(),
            session_ttl: std::time::Duration::from_secs(30 * 24 * 3600),
            allow_internal_urls: true,
        });
        (
            dir,
            AppState {
                db,
                secret_box: Arc::new(SecretBox::new(None).unwrap()),
                policy: OutboundPolicy::dev(),
                hub: Arc::new(Hub::new()),
                metrics: Arc::new(Metrics::default()),
                config,
            },
        )
    }

    async fn seed_admin_session(state: &AppState) -> crate::storage::sessions::Session {
        let hash = crate::security::password::hash_password("long-enough-password").unwrap();
        let u = crate::storage::users::create(&state.db, "admin@x.test", &hash, "admin")
            .await
            .unwrap();
        crate::storage::sessions::create(&state.db, &u.id, chrono::Duration::hours(1), None, None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn returns_decrypted_config_for_admin() {
        let (_dir, state) = app_state().await;
        let sess = seed_admin_session(&state).await;

        // Persist what the wizard save endpoints would store: encrypted secrets.
        let enc_pw = state
            .secret_box
            .encrypt_to_string(b"s3cret-password")
            .unwrap();
        let enc_key = state.secret_box.encrypt_to_string(b"tmdb-api-key").unwrap();
        let enc_ix_key = state.secret_box.encrypt_to_string(b"ix-api-key").unwrap();
        let enc_ix_pw = state.secret_box.encrypt_to_string(b"ix-password").unwrap();

        crate::storage::settings::upsert(
            &state.db,
            "setup.clients",
            &json!({"qbittorrent": {"url": "http://qbit:8080", "username": "admin", "passwordEnc": enc_pw}})
                .to_string(),
        )
        .await
        .unwrap();
        crate::storage::settings::upsert(
            &state.db,
            "setup.indexers",
            &json!({"torznab": [{"name": "ix", "url": "http://ix:9696", "username": "", "apiKeyEnc": enc_ix_key, "passwordEnc": enc_ix_pw}], "newznab": []})
                .to_string(),
        )
        .await
        .unwrap();
        crate::storage::settings::upsert(
            &state.db,
            "setup.metadata",
            &json!({"tmdb": {"baseUrl": "https://api.themoviedb.org", "apiKeyEnc": enc_key}})
                .to_string(),
        )
        .await
        .unwrap();
        crate::storage::settings::upsert(
            &state.db,
            "setup.policies",
            r#"{"language":"en","bannedTags":[],"qualityProfiles":[]}"#,
        )
        .await
        .unwrap();
        crate::storage::settings::upsert(
            &state.db,
            "setup.paths",
            r#"{"downloadsCompletedPath":"/dl","libraryRoots":["/lib"],"quarantinePath":"/q"}"#,
        )
        .await
        .unwrap();

        let resp = axum::Router::new()
            .nest("/api/v1", router())
            .layer(middleware::from_fn(csrf))
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/setup/config")
                    .header("cookie", format!("rivet_session={}", sess.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();

        // Secrets come back decrypted; nothing ciphertext-shaped leaks.
        assert_eq!(v["clients"]["qbittorrent"]["password"], "s3cret-password");
        assert_eq!(v["clients"]["qbittorrent"]["url"], "http://qbit:8080");
        assert_eq!(v["metadata"]["tmdb"]["apiKey"], "tmdb-api-key");
        assert_eq!(v["indexers"]["torznab"][0]["apiKey"], "ix-api-key");
        assert_eq!(v["indexers"]["torznab"][0]["password"], "ix-password");
        assert_eq!(v["paths"]["downloadsCompletedPath"], "/dl");
        assert_eq!(v["policies"]["language"], "en");

        // And the raw DB rows never contain the plaintext.
        let raw_clients = crate::storage::settings::get(&state.db, "setup.clients")
            .await
            .unwrap()
            .unwrap();
        assert!(
            !raw_clients.contains("s3cret-password"),
            "plaintext must not be stored"
        );
    }

    #[tokio::test]
    async fn requires_admin() {
        let (_dir, state) = app_state().await;
        let resp = axum::Router::new()
            .layer(middleware::from_fn(csrf))
            .nest("/api/v1", router())
            .layer(middleware::from_fn_with_state(state.clone(), load_session))
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/setup/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
