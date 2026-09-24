use crate::paths;
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_SHARD_BYTES: u64 = 5 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 35 * 1024 * 1024;
const RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Dependency-free JSONL logger. `event` stays compatible with existing callers.
pub struct Logger {
    lock: Mutex<()>,
    session_id: String,
}

impl Logger {
    pub fn new() -> Self {
        let session_id = format!("{}-{}", now_epoch(), std::process::id());
        Self {
            lock: Mutex::new(()),
            session_id,
        }
    }

    pub fn event(&self, level: &str, event: &str, detail: &str) {
        let _guard = self.lock.lock().ok();
        let dir = paths::log_dir();
        if fs::create_dir_all(&dir).is_err() {
            return;
        }
        let file = dir.join("current.log");
        if should_rotate(&file) {
            let rotated = dir.join(format!(
                "archive-{}-{}.jsonl",
                now_millis(),
                std::process::id()
            ));
            let _ = fs::rename(&file, rotated);
        }
        let record = json!({
            "ts": now_epoch(),
            "level": normalize_level(level),
            "event": sanitize_token(event),
            "detail": redact(detail),
            "version": env!("CARGO_PKG_VERSION"),
            "session_id": self.session_id,
        });
        if let Ok(mut handle) = OpenOptions::new().create(true).append(true).open(&file) {
            let _ = writeln!(handle, "{}", record);
        }
        cleanup(&dir);
    }

    /// Return the newest log data, useful for a status/debug panel.
    pub fn read(&self, max_bytes: usize) -> std::io::Result<String> {
        let _guard = self.lock.lock().ok();
        read_tail(&paths::log_dir().join("current.log"), max_bytes)
    }

    /// Copy all retained JSONL shards to a user-selected destination.
    pub fn export_to(&self, destination: &Path) -> std::io::Result<()> {
        let _guard = self.lock.lock().ok();
        let dir = paths::log_dir();
        let mut files = log_files(&dir);
        files.sort();
        let mut output = File::create(destination)?;
        for file in files {
            let mut input = File::open(file)?;
            std::io::copy(&mut input, &mut output)?;
        }
        Ok(())
    }

    pub fn log_dir(&self) -> PathBuf {
        paths::log_dir()
    }
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn normalize_level(value: &str) -> &str {
    if value.eq_ignore_ascii_case("ERROR") {
        "ERROR"
    } else if value.eq_ignore_ascii_case("WARN") || value.eq_ignore_ascii_case("WARNING") {
        "WARN"
    } else if value.eq_ignore_ascii_case("DEBUG") {
        "DEBUG"
    } else {
        "INFO"
    }
}

fn sanitize_token(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(80)
        .collect()
}

/// Remove secrets/control characters even if a caller passes an untrusted error.
fn redact(value: &str) -> String {
    let mut result = value.replace(['\n', '\r', '\t'], " ");
    let keys = [
        "password",
        "passwd",
        "uid",
        "token",
        "code",
        "cookie",
        "phone",
        "satoken",
        "authorization",
        "set-cookie",
        "apartmentid",
        "roomid",
    ];
    for key in keys {
        let mut offset = 0;
        loop {
            let lower = result.to_ascii_lowercase();
            let Some(found) = lower[offset..].find(key) else {
                break;
            };
            let start = offset + found;
            let boundary = start == 0 || !lower.as_bytes()[start - 1].is_ascii_alphanumeric();
            if !boundary {
                offset = start + key.len();
                continue;
            }
            let mut end = start + key.len();
            while end < result.len() && result.as_bytes()[end].is_ascii_whitespace() {
                end += 1;
            }
            if end < result.len()
                && (result.as_bytes()[end] == b'=' || result.as_bytes()[end] == b':')
            {
                end += 1;
                while end < result.len() && result.as_bytes()[end].is_ascii_whitespace() {
                    end += 1;
                }
                let value_end = result[end..]
                    .find(char::is_whitespace)
                    .map(|n| end + n)
                    .unwrap_or(result.len());
                result.replace_range(start..value_end, &format!("{key}=<redacted>"));
                offset = start + key.len() + 12;
            } else {
                offset = end;
            }
        }
    }
    if result.len() > 1000 {
        result.truncate(1000);
        result.push('…');
    }
    result
}

fn should_rotate(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.len() >= MAX_SHARD_BYTES)
        .unwrap_or(false)
}

fn log_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_string_lossy();
            (name == "current.log" || name.starts_with("archive-")).then_some(path)
        })
        .collect()
}

fn cleanup(dir: &Path) {
    let now = now_epoch();
    let mut files = log_files(dir);
    for path in &files {
        if path
            .file_name()
            .map(|n| n == "current.log")
            .unwrap_or(false)
        {
            continue;
        }
        let old = fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| now.saturating_sub(d.as_secs()) > RETENTION_SECONDS)
            .unwrap_or(false);
        if old {
            let _ = fs::remove_file(path);
        }
    }
    files = log_files(dir);
    let mut total: u64 = files
        .iter()
        .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
        .sum();
    if total <= MAX_TOTAL_BYTES {
        return;
    }
    files.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
    for path in files {
        if path
            .file_name()
            .map(|n| n == "current.log")
            .unwrap_or(false)
        {
            continue;
        }
        if total <= MAX_TOTAL_BYTES {
            break;
        }
        if let Ok(size) = fs::metadata(&path).map(|m| m.len()) {
            let _ = fs::remove_file(path);
            total = total.saturating_sub(size);
        }
    }
}

fn read_tail(path: &Path, max_bytes: usize) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    if data.len() > max_bytes {
        data = data.split_off(data.len() - max_bytes);
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}

#[cfg(test)]
mod tests {
    use super::redact;
    #[test]
    fn redact_secrets_and_newlines() {
        let value = redact("uid=abc token=secret\nnormal=yes");
        assert!(value.contains("uid=<redacted>"));
        assert!(value.contains("token=<redacted>"));
        assert!(!value.contains("secret"));
        assert!(!value.contains('\n'));
    }
}
