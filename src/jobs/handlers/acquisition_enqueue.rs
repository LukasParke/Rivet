//! `acquisition_enqueue`: push the candidate's download URL into the torrent
//! client and hand off to the monitor.
//!
//! Fixes over the Go original (`internal/jobs/handlers/acquisition_enqueue.go`):
//! - No fuzzy "find my torrent by title" matching: if neither a magnet btih
//!   nor an id returned by `add_url` identifies the torrent, the enqueue
//!   FAILS. The original could grab any unrelated torrent from the client.
//! - The qBittorrent adapter scopes adds and queries by category "rivet"
//!   (handled inside the adapter; nothing extra passed here).
//! - The rescout continuation on add failure propagates its enqueue error.

use crate::jobs::handlers::{extract_btih, load_torrent_client, publish};
use crate::jobs::{EnqueuePayload, MonitorPayload};
use crate::state::AppState;
use crate::storage::acquisitions;
use crate::storage::jobs::{self, Job};

pub async fn handle(state: &AppState, job: &Job) -> Result<(), String> {
    let payload: EnqueuePayload = parse_payload(job)?;
    if payload.acquisition_id.trim().is_empty() {
        return Err("invalid_payload".to_string());
    }

    let a = acquisitions::get(&state.db, &payload.acquisition_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("acquisition_not_found")?;

    // Load the candidate's raw JSON for the enclosure URL.
    // Candidates are stored as serialized `adapters::Release`; accept the
    // legacy wizard-era field names too.
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default, rename = "download_url")]
        download_url: String,
        #[serde(default, rename = "enclosureUrl")]
        enclosure_url: String,
        #[serde(default)]
        link: String,
    }
    let row: Option<(String,)> =
        sqlx::query_as("SELECT raw_json FROM release_candidates WHERE id = ?")
            .bind(&a.candidate_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    let (raw_json,) = row.ok_or("candidate_not_found")?;
    let raw: Raw = serde_json::from_str(&raw_json).unwrap_or(Raw {
        download_url: String::new(),
        enclosure_url: String::new(),
        link: String::new(),
    });
    let url = if !raw.download_url.trim().is_empty() {
        raw.download_url.trim()
    } else if !raw.enclosure_url.trim().is_empty() {
        raw.enclosure_url.trim()
    } else {
        raw.link.trim()
    };
    if url.is_empty() {
        return Err("candidate_missing_url".to_string());
    }

    let cli = load_torrent_client(state, &a.client_kind).await?;

    // Prefer a hash extracted from the magnet so tracking is exact.
    let magnet_hash = extract_btih(url);

    let add_result = cli.add_url(url).await;
    if let Err(e) = &add_result {
        acquisitions::mark_failed(&state.db, &a.id, &e.to_string())
            .await
            .map_err(|db| db.to_string())?;
        publish(
            state,
            "download.failed",
            serde_json::json!({
                "acquisitionId": a.id,
                "candidateId": a.candidate_id,
                "targetId": a.target_id,
                "clientKind": a.client_kind,
                "clientName": a.client_name,
                "error": e.to_string()
            }),
        );
        // Rescout for an alternative — propagation is mandatory.
        jobs::enqueue(
            &state.db,
            "acquisition_rescout",
            Some(&crate::jobs::RescoutPayload {
                target_id: a.target_id.clone(),
                failed_acquisition_id: Some(a.id.clone()),
            }),
            Some(chrono::Utc::now() + chrono::Duration::seconds(1)),
        )
        .await
        .map_err(|e| format!("enqueue_rescout_failed: {e}"))?;
        state.metrics.inc_jobs_enqueued();
        return Err(e.to_string());
    }

    // Identify the torrent EXACTLY: magnet btih or the client-returned id.
    // Fuzzy whole-client matching was a reviewed wrong-torrent bug and is
    // deliberately not reproduced here.
    let add_id = match &add_result {
        Ok(id) => id.clone(),
        Err(_) => None,
    };
    let external_id = resolve_external_id(magnet_hash.as_deref(), add_id);
    let external_id = match external_id {
        Some(id) => id,
        None => {
            let err = "no_hash_identifiable".to_string();
            acquisitions::mark_failed(&state.db, &a.id, &err)
                .await
                .map_err(|db| db.to_string())?;
            publish(
                state,
                "download.failed",
                serde_json::json!({
                    "acquisitionId": a.id,
                    "candidateId": a.candidate_id,
                    "targetId": a.target_id,
                    "reason": err
                }),
            );
            return Err(err);
        }
    };

    acquisitions::mark_enqueued(&state.db, &a.id, Some(&external_id), Some(url))
        .await
        .map_err(|e| e.to_string())?;
    publish(
        state,
        "download.enqueued",
        serde_json::json!({
            "acquisitionId": a.id,
            "candidateId": a.candidate_id,
            "targetId": a.target_id,
            "clientKind": a.client_kind,
            "clientName": a.client_name,
            "sourceUrl": url,
            "hash": external_id
        }),
    );

    jobs::enqueue(
        &state.db,
        "acquisition_monitor",
        Some(&MonitorPayload {
            acquisition_id: a.id.clone(),
            not_found_polls: 0,
            paused_polls: 0,
        }),
        Some(chrono::Utc::now() + chrono::Duration::seconds(2)),
    )
    .await
    .map_err(|e| format!("enqueue_monitor_failed: {e}"))?;
    state.metrics.inc_jobs_enqueued();
    Ok(())
}

/// Pure decision helper: the torrent is identified by the magnet's btih or by
/// the id the client returned at add time. Anything else fails.
fn resolve_external_id(magnet_hash: Option<&str>, add_id: Option<String>) -> Option<String> {
    if let Some(h) = magnet_hash.map(str::trim).filter(|h| !h.is_empty()) {
        return Some(h.to_string());
    }
    add_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_payload(job: &Job) -> Result<EnqueuePayload, String> {
    let raw = job.payload_json.as_deref().ok_or("missing_payload")?;
    serde_json::from_str(raw).map_err(|e| format!("invalid_payload: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_prefers_magnet_hash_then_add_id_then_none() {
        assert_eq!(
            resolve_external_id(Some(" ABC "), Some("xyz".into())).as_deref(),
            Some("ABC")
        );
        assert_eq!(
            resolve_external_id(None, Some(" client-id ".into())).as_deref(),
            Some("client-id")
        );
        assert_eq!(resolve_external_id(Some("  "), None), None);
        assert_eq!(resolve_external_id(None, None), None);
        assert_eq!(resolve_external_id(None, Some("   ".into())), None);
    }
}
