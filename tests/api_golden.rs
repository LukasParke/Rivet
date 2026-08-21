//! Golden test for the HTTP API route inventory.
//!
//! Scans `src/httpapi/mod.rs` and `src/httpapi/routes/*.rs` at runtime,
//! regex-extracts every `.route("path", method(handler))` registration, and
//! compares the resulting inventory against the committed snapshot in
//! `tests/golden/api/routes.json`. Modules merged under `/api/v1` in mod.rs
//! get the prefix; root-registered routes (healthz/readyz/metrics/ws/ws-info)
//! stay unprefixed.
//!
//! If this fails intentionally, regenerate with:
//!   `scripts/update-goldens.sh`  (or `GOLDEN_UPDATE=1 cargo test --test api_golden`)

use regex::Regex;
use serde_json::{json, Value};
use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn golden_path() -> PathBuf {
    manifest_dir().join("tests/golden/api/routes.json")
}

/// One `.route("path", <methods>)` registration.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Route {
    path: String,
    method: String,
}

/// Drop everything from the first `#[cfg(test)]` onward so unit-test-local
/// routers (health.rs, metrics.rs) never leak into the inventory.
fn strip_test_code(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(idx) => &src[..idx],
        None => src,
    }
}

/// Extract `(path, methods)` from every `.route(` call in `src`.
///
/// Handles multi-line registrations by paren-balancing from each `.route(`
/// to its closing paren; methods are found as `get(`/`post(`/... tokens
/// inside the argument expression (optionally behind `axum::routing::`).
fn extract_routes(src: &str) -> Vec<Route> {
    let path_re = Regex::new(r#""([^"]+)""#).unwrap();
    let method_re =
        Regex::new(r"(?:^|[^a-zA-Z_])(get|post|put|delete|patch|head|options)\s*\(").unwrap();
    let bytes = src.as_bytes();
    let mut routes = Vec::new();

    let mut search_from = 0;
    while let Some(rel) = src[search_from..].find(".route(") {
        let open = search_from + rel + ".route(".len();
        // Balance parens to find the closing paren of this .route( call.
        let mut depth = 1usize;
        let mut close = None;
        for (i, &b) in bytes[open..].iter().enumerate() {
            match b {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { break };
        let args = &src[open..close];

        let path = path_re
            .captures(args)
            .map(|c| c[1].to_string())
            .unwrap_or_else(|| panic!(".route( without a string literal path near byte {open}"));
        for cap in method_re.captures_iter(args) {
            routes.push(Route {
                path: path.clone(),
                method: cap[1].to_ascii_uppercase(),
            });
        }
        search_from = close;
    }
    routes
}

/// Module names merged via `routes::<name>::router()` within a slice of mod.rs.
fn merged_modules(text: &str) -> Vec<String> {
    let re = Regex::new(r"routes::([a-z_]+)::router\(\)").unwrap();
    re.captures_iter(text).map(|c| c[1].to_string()).collect()
}

/// Build the full sorted route inventory from the current source tree.
fn build_inventory() -> Vec<Route> {
    let mod_rs = std::fs::read_to_string(manifest_dir().join("src/httpapi/mod.rs"))
        .expect("read src/httpapi/mod.rs");
    assert!(
        mod_rs.contains(".nest(\"/api/v1\""),
        "mod.rs no longer nests under /api/v1 — update api_golden assumptions"
    );

    // The `/api/v1` sub-router spans `let api = Router::new()` up to the
    // root router's own `Router::new()` chain. Merges after that point are
    // root-registered (e.g. ws) and stay unprefixed.
    let api_start = mod_rs
        .find("let api = Router::new()")
        .expect("mod.rs api sub-router block not found");
    let root_start = api_start
        + mod_rs[api_start..]
            .find("\n    Router::new()")
            .expect("mod.rs root Router block not found");

    let prefixed: Vec<String> = merged_modules(&mod_rs[api_start..root_start]);
    let root_modules: Vec<String> = merged_modules(&mod_rs[root_start..]);

    // mod.rs's own direct registrations (healthz/readyz/metrics).
    let mut all = extract_routes(strip_test_code(&mod_rs));

    let routes_dir = manifest_dir().join("src/httpapi/routes");
    let module_file = |name: &str| routes_dir.join(format!("{name}.rs"));
    let read_module = |name: &str| {
        let p = module_file(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    };

    for name in &prefixed {
        for mut route in extract_routes(strip_test_code(&read_module(name))) {
            route.path = format!("/api/v1{}", route.path);
            all.push(route);
        }
    }
    for name in &root_modules {
        all.extend(extract_routes(strip_test_code(&read_module(name))));
    }

    all.sort();
    all.dedup();
    all
}

fn render_inventory(routes: &[Route]) -> String {
    let entries: Vec<Value> = routes
        .iter()
        .map(|r| json!({"method": r.method, "path": r.path}))
        .collect();
    serde_json::to_string_pretty(&json!({
        "_comment": "Committed API route inventory. Regenerate with scripts/update-goldens.sh",
        "routes": entries,
    }))
    .unwrap()
        + "\n"
}

#[test]
fn api_route_inventory_matches_golden() {
    let routes = build_inventory();
    assert!(
        !routes.is_empty(),
        "route extraction found nothing — parser is broken or routes moved"
    );
    let rendered = render_inventory(&routes);
    let path = golden_path();

    if std::env::var("GOLDEN_UPDATE").is_ok() || !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
    }

    let expected = std::fs::read_to_string(&path).unwrap();
    assert!(
        rendered == expected,
        "API route inventory drifted from tests/golden/api/routes.json.\n\
         Inspect the diff; if the change is intentional regenerate with:\n\
         ./scripts/update-goldens.sh   (or GOLDEN_UPDATE=1 cargo test --test api_golden)"
    );
}

#[test]
fn charte_openapi_is_v31_with_paths() {
    let doc_path: PathBuf = manifest_dir().join("packages/contracts/openapi.charte.json");
    let raw = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", doc_path.display()));
    let doc: Value = serde_json::from_str(&raw).expect("openapi.charte.json parses as JSON");

    let version = doc["openapi"].as_str().unwrap_or_else(|| {
        panic!("openapi.charte.json missing top-level `openapi` version string")
    });
    assert!(
        version.starts_with("3.1"),
        "expected OpenAPI 3.1.x document, got {version}"
    );

    let paths = doc["paths"].as_object().expect("`paths` object").len();
    assert!(
        paths >= 20,
        "expected >= 20 documented paths, got {paths} — charte generation regressed?"
    );
}
