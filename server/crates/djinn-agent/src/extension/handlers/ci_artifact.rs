//! Bounded, convention-free rendering for downloaded CI artifact ZIP files.
//! It consumes bytes only and never extracts a member to disk.

use std::io::{Cursor, Read};

const MAX_ENTRIES: usize = 256;
const MAX_ENTRY_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const BINARY_INSPECTION_BYTES: usize = 8 * 1024;
const MAX_LINES_PER_FILE: usize = 1_000;
const MAX_RENDERED_FILE_BYTES: usize = 64 * 1024;
const MAX_REPORT_BYTES: usize = 2 * 1024 * 1024;

/// Render a ZIP artifact wholly in memory, enforcing limits while reading.
pub(crate) fn render_ci_artifact_zip(bytes: &[u8]) -> Result<String, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("invalid ZIP artifact: {error}"))?;
    if archive.len() > MAX_ENTRIES {
        return Err(format!("ZIP artifact has more than {MAX_ENTRIES} entries"));
    }
    let mut total = 0usize;
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|e| format!("failed to read ZIP entry {index}: {e}"))?;
        let path = normalize_path(file.name())?;
        if file.is_symlink() {
            return Err(format!("ZIP artifact contains symlink entry: {path}"));
        }
        if file.is_dir() {
            entries.push(RenderEntry::Skipped(path, "directory"));
            continue;
        }
        let mut body = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            let read = file
                .read(&mut chunk)
                .map_err(|e| format!("failed to decompress ZIP entry {path}: {e}"))?;
            if read == 0 {
                break;
            }
            if body.len().saturating_add(read) > MAX_ENTRY_BYTES {
                return Err(format!("ZIP entry exceeds {MAX_ENTRY_BYTES} bytes: {path}"));
            }
            total = total.saturating_add(read);
            if total > MAX_TOTAL_BYTES {
                return Err(format!(
                    "ZIP artifact exceeds {MAX_TOTAL_BYTES} decompressed bytes"
                ));
            }
            body.extend_from_slice(&chunk[..read]);
        }
        let inspected = &body[..body.len().min(BINARY_INSPECTION_BYTES)];
        entries.push(if body.contains(&0) {
            RenderEntry::Skipped(path, "NUL byte content")
        } else if is_binary(inspected) {
            RenderEntry::Skipped(path, "binary content")
        } else if std::str::from_utf8(&body).is_err() {
            RenderEntry::Skipped(path, "invalid UTF-8 content")
        } else {
            RenderEntry::Text(path, String::from_utf8(body).expect("validated UTF-8"))
        });
    }
    Ok(render_entries(entries))
}

enum RenderEntry {
    Text(String, String),
    Skipped(String, &'static str),
}

fn normalize_path(name: &str) -> Result<String, String> {
    if name.is_empty() || name.contains('\0') || name.starts_with('/') || name.starts_with('\\') {
        return Err(format!("ZIP artifact contains unsafe path: {name:?}"));
    }
    let mut parts = Vec::new();
    for part in name.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        let raw = part.as_bytes();
        if part == ".." || (raw.len() >= 2 && raw[0].is_ascii_alphabetic() && raw[1] == b':') {
            return Err(format!("ZIP artifact contains unsafe path: {name:?}"));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(format!("ZIP artifact contains unsafe path: {name:?}"));
    }
    Ok(parts.join("/"))
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|b| matches!(*b, 1..=8 | 11..=12 | 14..=31))
}

fn render_entries(entries: Vec<RenderEntry>) -> String {
    let mut report = String::new();
    let mut omitted = Vec::new();
    let mut stopped = false;
    for entry in entries {
        let (path, body) = match entry {
            RenderEntry::Text(p, t) => (p, render_file_text(&t)),
            RenderEntry::Skipped(p, r) => (p, format!("[skipped: {r}]\n")),
        };
        let heading = format!("== {path} ==\n");
        if stopped
            || report
                .len()
                .saturating_add(heading.len())
                .saturating_add(body.len())
                > MAX_REPORT_BYTES
        {
            stopped = true;
            omitted.push(path);
        } else {
            report.push_str(&heading);
            report.push_str(&body);
        }
    }
    if !omitted.is_empty() {
        push_bounded(&mut report, "\n[report limit reached; omitted paths:]\n");
        for path in omitted {
            push_bounded(&mut report, "- ");
            push_bounded(&mut report, &path);
            push_bounded(&mut report, "\n");
        }
    }
    report
}

fn render_file_text(text: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let omitted_lines = lines.len().saturating_sub(MAX_LINES_PER_FILE);
    let content = lines[lines.len().saturating_sub(MAX_LINES_PER_FILE)..].concat();
    let mut output = if omitted_lines == 0 {
        String::new()
    } else {
        format!("[omitted {omitted_lines} leading lines]\n")
    };
    if output.len() + content.len() <= MAX_RENDERED_FILE_BYTES {
        output.push_str(&content);
        return output;
    }
    let marker = "[omitted leading bytes]\n";
    let budget = MAX_RENDERED_FILE_BYTES.saturating_sub(output.len() + marker.len());
    let mut start = content.len().saturating_sub(budget);
    while start < content.len() && !content.is_char_boundary(start) {
        start += 1;
    }
    output.push_str(marker);
    output.push_str(&content[start..]);
    output
}

fn push_bounded(target: &mut String, value: &str) {
    let mut end = value
        .len()
        .min(MAX_REPORT_BYTES.saturating_sub(target.len()));
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&value[..end]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    fn zip(entries: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (n, b) in entries {
            w.start_file(n, SimpleFileOptions::default()).unwrap();
            w.write_all(&b).unwrap();
        }
        w.finish().unwrap().into_inner()
    }
    #[test]
    fn order_and_binary_metadata() {
        let report = render_ci_artifact_zip(&zip(vec![
            ("a", b"one\n".to_vec()),
            ("b", vec![0, b'x']),
            ("c", b"three\n".to_vec()),
        ]))
        .unwrap();
        assert!(report.find("== a").unwrap() < report.find("== b").unwrap());
        assert!(report.contains("[skipped: NUL byte content]"));
        assert!(!report.contains("== b ==\nx"));
    }
    #[test]
    fn unsafe_paths_fail_whole_archive() {
        for name in ["../bad", "/bad", "C:/bad", "safe/../../bad"] {
            assert!(render_ci_artifact_zip(&zip(vec![(name, b"x".to_vec())])).is_err());
        }
    }
    #[test]
    fn keeps_last_thousand_lines() {
        let text = (0..1001).map(|n| format!("{n}\n")).collect::<String>();
        let report = render_ci_artifact_zip(&zip(vec![("log", text.into_bytes())])).unwrap();
        assert!(report.contains("[omitted 1 leading lines]"));
        assert!(report.contains("1000\n"));
    }
}
