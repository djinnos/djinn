//! Process-global, durable panic evidence.
use std::backtrace::Backtrace;
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use serde_json::json;

pub const PANIC_SUMMARY_MAX_BYTES: usize = 1024;
const MESSAGE_MAX_BYTES: usize = 384;
const FILE_MAX_BYTES: usize = 256;
const FALLBACK_SUMMARY: &str = "{\"event\":\"djinn.panic_summary.v1\",\"message\":\"panic summary serialization failed\",\"file\":null,\"line\":null,\"column\":null,\"backtrace_truncated\":true}";
static INSTALLED: Once = Once::new();
static IN_HOOK: AtomicBool = AtomicBool::new(false);

/// Install the previous-hook-preserving panic capture once for this process.
pub fn install() {
    INSTALLED.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if IN_HOOK.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() { return; }
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = panic::catch_unwind(AssertUnwindSafe(|| previous(info)));
                let details = PanicDetails::from_hook(info);
                let backtrace = Backtrace::force_capture().to_string();
                let _ = panic::catch_unwind(AssertUnwindSafe(|| write_backtrace(&details, &backtrace)));
                let _ = panic::catch_unwind(AssertUnwindSafe(|| write_summary(&details)));
            }));
            if outcome.is_err() { write_line(FALLBACK_SUMMARY); }
            IN_HOOK.store(false, Ordering::Release);
        }));
    });
}

#[derive(Clone)]
struct PanicDetails { message: String, file: Option<String>, line: Option<u32>, column: Option<u32> }
impl PanicDetails {
    fn from_hook(info: &PanicHookInfo<'_>) -> Self {
        let message = if let Some(v) = info.payload().downcast_ref::<&str>() { (*v).to_owned() }
        else if let Some(v) = info.payload().downcast_ref::<String>() { v.clone() }
        else { "non-string panic payload".to_owned() };
        let location = info.location();
        Self { message: truncate_utf8(&message, MESSAGE_MAX_BYTES), file: location.map(|v| truncate_utf8(v.file(), FILE_MAX_BYTES)), line: location.map(std::panic::Location::line), column: location.map(std::panic::Location::column) }
    }
}
fn write_backtrace(details: &PanicDetails, backtrace: &str) {
    let record = json!({"event":"djinn.panic_backtrace.v1","message":details.message,"file":details.file,"line":details.line,"column":details.column,"backtrace":backtrace});
    if let Ok(encoded) = serde_json::to_string(&record) { write_line(&encoded); }
}
fn write_summary(details: &PanicDetails) { write_line(&encode_summary(details)); }
fn encode_summary(details: &PanicDetails) -> String {
    let record = json!({"event":"djinn.panic_summary.v1","message":details.message,"file":details.file,"line":details.line,"column":details.column,"backtrace_truncated":false});
    match serde_json::to_string(&record) { Ok(encoded) if encoded.len() <= PANIC_SUMMARY_MAX_BYTES => encoded, _ => FALLBACK_SUMMARY.to_owned() }
}
#[allow(clippy::print_stderr)]
fn write_line(record: &str) { eprintln!("{record}"); }
fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes { return value.to_owned(); }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) { end -= 1; }
    value[..end].to_owned()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chained_large_hook() {
        let details = PanicDetails { message: truncate_utf8(&"🙂".repeat(200), MESSAGE_MAX_BYTES), file: Some(truncate_utf8(&"é".repeat(300), FILE_MAX_BYTES)), line: Some(12), column: Some(34) };
        let encoded = encode_summary(&details);
        assert!(encoded.len() <= PANIC_SUMMARY_MAX_BYTES);
        let parsed: serde_json::Value = serde_json::from_str(&encoded).expect("valid JSON");
        assert_eq!(parsed["event"], "djinn.panic_summary.v1");
        assert_eq!(parsed["line"], 12);
    }
    #[test]
    fn utf8_truncation_never_splits_a_code_point() { assert_eq!(truncate_utf8("abc🙂def", 6), "abc"); }
}
