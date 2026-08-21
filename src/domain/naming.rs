//! *arr-grade file-naming template engine.
//!
//! Renders naming templates such as
//! `{Title} - S{Season:00}E{Episode:00} - {Quality Full}` against a
//! [`NamingContext`], mirroring Sonarr/Radarr behaviour:
//!
//! - Unknown tokens are removed and leftover separators cleaned up, so
//!   `Show - {Bogus}` renders as `Show` instead of `Show - `.
//! - Numeric tokens accept a zero-padding spec (`{Season:00}`, `{Episode:000}`).
//! - With `safe_for_windows`, characters illegal on Windows filenames are
//!   replaced (`:` becomes `" - "`, everything else `-`), and trailing
//!   dots/spaces are always trimmed.
//! - Multi-episode files join as `E01-E02`; [`render_series`] picks a
//!   dedicated multi-episode template when one is needed.

use serde::{Deserialize, Serialize};

/// Values available to naming templates. Serializes with camelCase field
/// names so wire JSON mirrors the *arr API (`episodeTitles`, `qualityLabel`,
/// `airDate`, ...). All fields default, so callers build only what they know.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NamingContext {
    /// Primary title (series title or movie title).
    pub title: String,
    pub year: Option<i64>,
    pub season: Option<i64>,
    /// Ordered episode numbers; more than one entry marks a multi-episode file.
    pub episodes: Vec<i64>,
    /// Per-episode titles aligned with `episodes` where known.
    pub episode_titles: Vec<String>,
    /// Full quality label, e.g. `WEB-DL 1080p`; empty means unknown.
    pub quality_label: String,
    pub languages: Vec<String>,
    pub group: Option<String>,
    /// Edition tags such as `Extended Cut` (movies).
    pub edition: Option<String>,
    pub original_title: Option<String>,
    /// Air date as an ISO `yyyy-MM-dd` string.
    pub air_date: Option<String>,
}

impl NamingContext {
    /// Context with only the primary title set; extend with struct-update
    /// syntax (`..Default::default()` is already applied).
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), ..Self::default() }
    }
}

/// Characters Windows forbids in file names, apart from `:` which gets the
/// *arr-style `" - "` replacement.
const WINDOWS_ILLEGAL: &[char] = &['<', '>', '"', '\\', '/', '|', '?', '*'];

/// Zero-pads `n` to at least `width` digits when a positive width is given.
fn pad(n: i64, width: Option<usize>) -> String {
    match width {
        Some(w) if w > 0 => format!("{n:0>w$}"),
        _ => n.to_string(),
    }
}

/// Parses `{Season:00}`-style specs: each `0` in the spec means one
/// minimum digit (`"00"` pads to 2, `"000"` to 3). Non-numeric or empty
/// specs mean no padding.
fn padding_spec(spec: Option<&str>) -> Option<usize> {
    let s = spec?;
    (!s.is_empty() && s.bytes().all(|b| b == b'0')).then_some(s.len())
}

fn episode_value(ctx: &NamingContext, width: Option<usize>) -> Option<String> {
    let mut eps = ctx.episodes.iter();
    let first = pad(*eps.next()?, width);
    if ctx.episodes.len() == 1 {
        return Some(first);
    }
    let mut joined = first;
    for e in eps {
        joined.push_str("-E");
        joined.push_str(&pad(*e, width));
    }
    Some(joined)
}

/// Resolves one token body (name + optional `:spec`) to its value.
/// `None` means unknown token or missing optional value: substituted away.
fn resolve_token(name: &str, spec: Option<&str>, ctx: &NamingContext) -> Option<String> {
    match name {
        "Title" => (!ctx.title.is_empty()).then(|| ctx.title.clone()),
        "title" => (!ctx.title.is_empty()).then(|| ctx.title.to_lowercase()),
        "Original Title" => ctx.original_title.clone().filter(|s| !s.is_empty()),
        "Year" => ctx.year.map(|y| y.to_string()),
        "Season" => ctx.season.map(|s| pad(s, padding_spec(spec))),
        // Bare `{Episode}` still yields standard 2-digit numbering (E01).
        "Episode" => {
            let width = spec.map(|s| padding_spec(Some(s))).unwrap_or(Some(2));
            episode_value(ctx, width)
        }
        "Episode Title" => {
            let titles: Vec<&str> =
                ctx.episode_titles.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect();
            (!titles.is_empty()).then(|| titles.join(" "))
        }
        "Quality Full" => (!ctx.quality_label.is_empty()).then(|| ctx.quality_label.clone()),
        "Languages" => {
            let langs: Vec<&str> = ctx.languages.iter().map(String::as_str).collect();
            (!langs.is_empty()).then(|| langs.join("-"))
        }
        "Release Group" => ctx.group.clone().filter(|s| !s.is_empty()),
        "Edition Tags" => ctx.edition.clone().filter(|s| !s.is_empty()),
        "Air Date" => ctx.air_date.clone().filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// Whether the template contains exactly the given token name (so
/// `{Episode Title}` does not count for `"Episode"`).
fn contains_token(template: &str, wanted: &str) -> bool {
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let Some(end_rel) = rest[start..].find('}') else { break };
        let inner = &rest[start + 1..start + end_rel];
        let name = inner.split(':').next().unwrap_or(inner).trim();
        if name == wanted {
            return true;
        }
        rest = &rest[start + end_rel + 1..];
    }
    false
}

/// Cleans up a rendered string like *arr does after removing tokens:
/// replaces illegal characters when `safe_for_windows`, collapses duplicate
/// separators left by empty components, and trims dangling edges.
fn clean(mut s: String, safe_for_windows: bool) -> String {
    if safe_for_windows {
        let mut replaced = String::with_capacity(s.len());
        for ch in s.chars() {
            match ch {
                ':' => replaced.push_str(" - "),
                c if WINDOWS_ILLEGAL.contains(&c) => replaced.push('-'),
                c => replaced.push(c),
            }
        }
        s = replaced;
    }
    // Fixpoint collapse of separators orphaned by removed/empty tokens.
    loop {
        let next = s
            .replace("()", "")
            .replace("( )", "")
            .replace("  ", " ")
            .replace("--", "-")
            .replace("- -", "-");
        if next == s {
            break;
        }
        s = next;
    }
    s.trim_matches([' ', '-', '.']).to_owned()
}

/// Renders `template` against `ctx`. Unknown tokens and missing optional
/// values vanish with their surrounding separators; see module docs.
pub fn render(template: &str, ctx: &NamingContext, safe_for_windows: bool) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        match rest[start..].find('}') {
            Some(end_rel) => {
                let inner = &rest[start + 1..start + end_rel];
                let (name, spec) = match inner.split_once(':') {
                    Some((n, s)) => (n.trim(), Some(s)),
                    None => (inner.trim(), None),
                };
                if let Some(value) = resolve_token(name, spec, ctx) {
                    out.push_str(&value);
                }
                rest = &rest[start + end_rel + 1..];
            }
            None => {
                // Unterminated brace: keep the remainder verbatim.
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    clean(out, safe_for_windows)
}

/// Renders a series filename, choosing between the standard template and the
/// multi-episode override. The multi template is used when the context holds
/// several episodes and the standard template actually places episodes via
/// `{Episode}` — e.g. standard `{Title} - S{Season:00}E{Episode:00}` with
/// multi `{Title} - S{Season:00}E{Episode}` yields `S01E01-E02` joins.
pub fn render_series(
    standard: &str,
    multi_episode_format: &str,
    ctx: &NamingContext,
    safe_for_windows: bool,
) -> String {
    let use_multi =
        ctx.episodes.len() > 1 && !multi_episode_format.is_empty() && contains_token(standard, "Episode");
    let template = if use_multi { multi_episode_format } else { standard };
    render(template, ctx, safe_for_windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct GoldenCase {
        name: String,
        template: Option<String>,
        standard: Option<String>,
        multi: Option<String>,
        context: NamingContext,
        safe: bool,
        expected: String,
    }

    const CASES_JSON: &str = include_str!("../../tests/golden/naming/cases.json");

    #[test]
    fn golden_naming_cases() {
        let cases: Vec<GoldenCase> =
            serde_json::from_str(CASES_JSON).expect("golden cases must parse");
        assert!(cases.len() >= 40, "expected >=40 golden cases, got {}", cases.len());
        for case in &cases {
            let got = match (&case.standard, &case.multi) {
                (Some(standard), multi) => render_series(
                    standard,
                    multi.as_deref().unwrap_or(""),
                    &case.context,
                    case.safe,
                ),
                _ => render(
                    case.template.as_deref().expect("case needs template or standard"),
                    &case.context,
                    case.safe,
                ),
            };
            assert_eq!(got, case.expected, "golden case `{}` failed", case.name);
        }
    }

    #[test]
    fn context_serializes_camel_case() {
        let ctx = NamingContext {
            episode_titles: vec!["Pilot".into()],
            quality_label: "DVD".into(),
            air_date: Some("2013-09-17".into()),
            ..NamingContext::new("Brooklyn Nine-Nine")
        };
        let json = serde_json::to_value(&ctx).unwrap();
        for key in ["episodeTitles", "qualityLabel", "airDate"] {
            assert!(json.get(key).is_some(), "missing camelCase key {key}");
        }
        let round: NamingContext = serde_json::from_value(json).unwrap();
        assert_eq!(round, ctx);
    }
}
