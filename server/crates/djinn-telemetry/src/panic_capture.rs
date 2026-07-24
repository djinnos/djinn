//! Process-global, durable panic evidence.
use serde_json::json;
use std::backtrace::Backtrace;
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

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
            if IN_HOOK
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = panic::catch_unwind(AssertUnwindSafe(|| previous(info)));
                let details = PanicDetails::from_hook(info);
                let backtrace = Backtrace::force_capture().to_string();
                let _ =
                    panic::catch_unwind(AssertUnwindSafe(|| write_backtrace(&details, &backtrace)));
                let _ = panic::catch_unwind(AssertUnwindSafe(|| write_summary(&details)));
            }));
            if outcome.is_err() {
                write_line(FALLBACK_SUMMARY);
            }
            IN_HOOK.store(false, Ordering::Release);
        }));
    });
}

#[derive(Clone)]
struct PanicDetails {
    message: String,
    file: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
}
impl PanicDetails {
    fn from_hook(info: &PanicHookInfo<'_>) -> Self {
        let message = if let Some(v) = info.payload().downcast_ref::<&str>() {
            (*v).to_owned()
        } else if let Some(v) = info.payload().downcast_ref::<String>() {
            v.clone()
        } else {
            "non-string panic payload".to_owned()
        };
        let location = info.location();
        Self {
            message: truncate_utf8(&message, MESSAGE_MAX_BYTES),
            file: location.map(|v| truncate_utf8(v.file(), FILE_MAX_BYTES)),
            line: location.map(std::panic::Location::line),
            column: location.map(std::panic::Location::column),
        }
    }
}
fn write_backtrace(details: &PanicDetails, backtrace: &str) {
    let record = json!({"event":"djinn.panic_backtrace.v1","message":details.message,"file":details.file,"line":details.line,"column":details.column,"backtrace":backtrace});
    if let Ok(encoded) = serde_json::to_string(&record) {
        write_line(&encoded);
    }
}
fn write_summary(details: &PanicDetails) {
    write_line(&encode_summary(details));
}
fn encode_summary(details: &PanicDetails) -> String {
    let record = json!({"event":"djinn.panic_summary.v1","message":details.message,"file":details.file,"line":details.line,"column":details.column,"backtrace_truncated":false});
    match serde_json::to_string(&record) {
        Ok(encoded) if encoded.len() <= PANIC_SUMMARY_MAX_BYTES => encoded,
        _ => FALLBACK_SUMMARY.to_owned(),
    }
}
#[allow(clippy::print_stderr)]
fn write_line(record: &str) {
    eprintln!("{record}");
}
fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
#[cfg(test)]
use std::process::Command;

#[cfg(test)]
const ATTEMPT_CAPTURE_BYTES: usize = 8_000;
#[cfg(test)]
const PREVIOUS_HOOK_MARKER: &str = "previous-hook-large-write:";

#[cfg(test)]
#[test]
#[allow(clippy::print_stderr)]
fn chained_large_hook() {
    // `install` is process-global. Exercise it in a separate libtest process
    // so this fixture can install a deliberately large previous hook.
    if std::env::var_os("DJINN_PANIC_CAPTURE_CHILD").is_some() {
        panic::set_hook(Box::new(|_| {
            eprintln!("{PREVIOUS_HOOK_MARKER}{}", "P".repeat(9 * 1024));
        }));
        install();
        deep_panic(256);
    }
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "panic_capture::chained_large_hook",
            "--nocapture",
        ])
        .env("RUST_BACKTRACE", "1")
        .env("DJINN_PANIC_CAPTURE_CHILD", "1")
        .output()
        .expect("run panic hook child");
    assert!(!output.status.success(), "child must panic");

    let durable = String::from_utf8(output.stderr).expect("panic output is UTF-8");
    let previous_offset = durable
        .find(PREVIOUS_HOOK_MARKER)
        .expect("previous hook fired");
    let backtrace_offset = durable
        .find("\"event\":\"djinn.panic_backtrace.v1\"")
        .expect("Djinn backtrace record was durable");
    let summary_offset = durable
        .find("\"event\":\"djinn.panic_summary.v1\"")
        .expect("Djinn summary record was durable");
    assert!(previous_offset < backtrace_offset && backtrace_offset < summary_offset);
    assert!(
        durable[previous_offset..]
            .lines()
            .next()
            .expect("previous hook line")
            .len()
            > 8 * 1024,
        "previous hook wrote >8 KiB"
    );

    let backtrace_line = durable
        .lines()
        .find(|line| line.contains("\"event\":\"djinn.panic_backtrace.v1\""))
        .expect("complete backtrace record");
    let backtrace: serde_json::Value =
        serde_json::from_str(backtrace_line).expect("valid backtrace JSON");
    assert!(
        backtrace["backtrace"]
            .as_str()
            .expect("backtrace string")
            .len()
            > 8 * 1024
    );

    let attempt_capture = utf8_tail(&durable, ATTEMPT_CAPTURE_BYTES);
    let summary_line = attempt_capture
        .lines()
        .find(|line| line.contains("\"event\":\"djinn.panic_summary.v1\""))
        .expect("final summary remains in 8000-byte attempt capture");
    let summary: serde_json::Value =
        serde_json::from_str(summary_line).expect("valid summary JSON");
    assert_eq!(summary["event"], "djinn.panic_summary.v1");
    assert!(summary_line.len() <= PANIC_SUMMARY_MAX_BYTES);
    assert!(
        durable[summary_offset..]
            .lines()
            .filter(|line| line.contains("\"event\":\"djinn.panic_"))
            .all(|line| line.contains("\"event\":\"djinn.panic_summary.v1\""))
    );
}

#[cfg(test)]
#[inline(never)]
// `black_box` deliberately wraps the recursive unit call to keep the deep frame
// stack intact for the fixture; the unit argument is the point, not a mistake.
#[allow(clippy::unit_arg)]
fn deep_panic(depth: usize) {
    if depth == 0 {
        panic!("panic capture fixture");
    }
    std::hint::black_box(deep_panic(depth - 1));
}

#[cfg(test)]
fn utf8_tail(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}
#[cfg(test)]
#[test]
fn utf8_truncation_never_splits_a_code_point() {
    assert_eq!(truncate_utf8("abc🙂def", 6), "abc");
}
