use djinn_db::ProjectRepository;

use super::manager::Inner;

pub(crate) fn project_repo(inner: &Inner) -> ProjectRepository {
    ProjectRepository::new(
        inner.db.clone(),
        crate::events::event_bus_for(&inner.events_tx),
    )
}

/// Current UTC time as an ISO-8601 string (second precision).
pub fn now_utc() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, m, s) = unix_to_ymd_hms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

pub(crate) fn unix_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let m = ((secs / 60) % 60) as u32;
    let h = ((secs / 3600) % 24) as u32;
    let mut days = secs / 86_400;

    let mut y = 1970u32;
    loop {
        let days_in_year: u64 =
            if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) {
                366
            } else {
                365
            };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        y += 1;
    }

    let leap = y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let month_days: [u32; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 1u32;
    let mut d = days as u32;
    for &dim in &month_days {
        if d < dim {
            d += 1;
            break;
        }
        d -= dim;
        mo += 1;
    }

    (y, mo, d, h, m, s)
}
