use djinn_core::events::EventBus;
use djinn_db::Database;
use std::sync::Arc;

use crate::oauth::github_app::GITHUB_APP_OAUTH_DB_KEY;
use crate::repos::CredentialRepository;

mod checks;
mod pull_requests;
mod transport;

pub fn make_repo() -> Arc<CredentialRepository> {
    let db = Database::open_in_memory().expect("in-memory db");
    Arc::new(CredentialRepository::new(db, EventBus::noop()))
}

pub async fn seed_tokens(repo: &CredentialRepository, access_token: &str) {
    let json = serde_json::json!({
        "access_token": access_token,
        "user_login": "djinn-test",
    })
    .to_string();
    repo.set("github_app", GITHUB_APP_OAUTH_DB_KEY, &json)
        .await
        .unwrap();
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn epoch_days_to_ymd(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn chrono_parse_iso8601(s: &str) -> anyhow::Result<i64> {
    let s = s.trim_end_matches('Z');
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!("invalid ISO-8601: {}", s));
    }
    let date_parts: Vec<u32> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    let time_parts: Vec<u32> = parts[1].split(':').filter_map(|p| p.parse().ok()).collect();
    if date_parts.len() != 3 || time_parts.len() != 3 {
        return Err(anyhow::anyhow!("invalid ISO-8601 parts: {}", s));
    }
    let (y, mo, d) = (
        date_parts[0] as i64,
        date_parts[1] as i64,
        date_parts[2] as i64,
    );
    let (h, mi, sec) = (
        time_parts[0] as i64,
        time_parts[1] as i64,
        time_parts[2] as i64,
    );

    let days = days_since_epoch(y, mo, d);
    Ok(days * 86400 + h * 3600 + mi * 60 + sec)
}

fn days_since_epoch(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = month;
    let d = day;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[test]
fn iso8601_parse_known_date() {
    let result = chrono_parse_iso8601("2024-01-01T00:00:00Z").unwrap();
    assert_eq!(result, 1_704_067_200);
}

#[test]
fn iso8601_parse_roundtrip() {
    let ts = now_secs();
    let days = ts / 86400;
    let rem = ts % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (year, month, day) = epoch_days_to_ymd(days);
    let formatted = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, h, m, s
    );
    let parsed = chrono_parse_iso8601(&formatted).unwrap();
    assert!(
        (parsed - ts).abs() <= 1,
        "round-trip failed: {} vs {}",
        parsed,
        ts
    );
}
