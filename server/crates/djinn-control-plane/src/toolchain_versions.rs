//! Live toolchain-version lists for the image version selectors.
//!
//! Fetches available versions from upstream (go.dev / nodejs.org /
//! endoflife.date) so the UI's per-language version dropdowns stay current
//! without manual list maintenance. Results are cached in-process with a TTL;
//! every source degrades to a sane static fallback on any error/timeout so the
//! UI always has options.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use std::sync::OnceLock;

use djinn_core::clock::{Clock, SystemClock};
use djinn_provider::http_util::HttpClient;

const TTL: Duration = Duration::from_secs(6 * 3600);
const FETCH_TIMEOUT: Duration = Duration::from_secs(4);

// Static fallbacks (also the values for languages without an upstream feed).
fn fallback(lang: &str) -> Vec<String> {
    let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match lang {
        "rust" => v(&["stable", "beta", "nightly"]),
        "node" => v(&["lts", "24", "22", "20"]),
        "python" => v(&["3.13", "3.12", "3.11"]),
        "go" => v(&["1.26", "1.25", "1.24"]),
        "java" => v(&["23", "21", "17"]),
        "ruby" => v(&["3.4", "3.3", "3.2"]),
        "dotnet" => v(&["9.0", "8.0"]),
        "clang" => v(&["19", "18", "17"]),
        _ => Vec::new(),
    }
}

const LANGS: [&str; 8] = [
    "rust", "node", "python", "go", "java", "ruby", "dotnet", "clang",
];

type Cache = Mutex<Option<(Instant, BTreeMap<String, Vec<String>>)>>;
fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Return available versions per language. Cached (TTL); fetches upstream on a
/// cold/stale cache, falling back to static lists per source on error.
pub async fn fetch_toolchain_versions() -> BTreeMap<String, Vec<String>> {
    if let Ok(guard) = cache().lock()
        && let Some((at, map)) = guard.as_ref()
        && at.elapsed() < TTL
    {
        return map.clone();
    }

    let client = HttpClient::new(FETCH_TIMEOUT).ok();

    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for lang in LANGS {
        let fetched = match (lang, client.as_ref()) {
            ("go", Some(c)) => fetch_go(c).await,
            ("node", Some(c)) => fetch_node(c).await,
            ("python", Some(c)) => fetch_endoflife(c, "python").await,
            ("java", Some(c)) => fetch_endoflife(c, "java").await,
            ("ruby", Some(c)) => fetch_endoflife(c, "ruby").await,
            ("dotnet", Some(c)) => fetch_endoflife(c, "dotnet").await,
            _ => None,
        };
        map.insert(lang.to_string(), fetched.unwrap_or_else(|| fallback(lang)));
    }

    if let Ok(mut guard) = cache().lock() {
        *guard = Some((SystemClock::new().now_instant(), map.clone()));
    }
    map
}

/// go.dev/dl: array of {version:"go1.26.0", stable:bool}. Returns distinct
/// major.minor of recent stable releases, newest first (capped).
async fn fetch_go(client: &HttpClient) -> Option<Vec<String>> {
    let json = client.get_json("https://go.dev/dl/?mode=json").await?;
    let arr = json.as_array()?;
    let mut out: Vec<String> = Vec::new();
    for entry in arr {
        if entry.get("stable").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let ver = entry.get("version").and_then(|v| v.as_str())?;
        let ver = ver.strip_prefix("go").unwrap_or(ver);
        // major.minor only (e.g. "1.26").
        let mm: String = {
            let mut it = ver.split('.');
            match (it.next(), it.next()) {
                (Some(a), Some(b)) => format!("{a}.{b}"),
                _ => ver.to_string(),
            }
        };
        if !out.contains(&mm) {
            out.push(mm);
        }
        if out.len() >= 6 {
            break;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// nodejs.org/dist: array of {version:"vX.Y.Z", lts:false|name}. Returns
/// "lts" + recent distinct majors.
async fn fetch_node(client: &HttpClient) -> Option<Vec<String>> {
    let json = client
        .get_json("https://nodejs.org/dist/index.json")
        .await?;
    let arr = json.as_array()?;
    let mut out: Vec<String> = vec!["lts".to_string()];
    for entry in arr {
        let ver = entry.get("version").and_then(|v| v.as_str())?;
        let major = ver.trim_start_matches('v').split('.').next()?;
        // Even majors are LTS lines; surface a handful newest-first.
        if !out.iter().any(|x| x == major) {
            out.push(major.to_string());
        }
        if out.len() >= 6 {
            break;
        }
    }
    (out.len() > 1).then_some(out)
}

/// endoflife.date/api/<product>.json: array of {cycle:"3.13", ...}. Returns
/// the recent cycles, newest first (capped).
async fn fetch_endoflife(client: &HttpClient, product: &str) -> Option<Vec<String>> {
    let url = format!("https://endoflife.date/api/{product}.json");
    let json = client.get_json(&url).await?;
    let arr = json.as_array()?;
    let out: Vec<String> = arr
        .iter()
        .filter_map(|e| e.get("cycle").and_then(|v| v.as_str()).map(String::from))
        .take(6)
        .collect();
    (!out.is_empty()).then_some(out)
}
