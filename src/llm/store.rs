use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const BASE_DIR: &str = "/tmp/tmux-agent-sidebar-names";

pub fn base_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("TMUX_AGENT_SIDEBAR_NAMES_DIR") {
        PathBuf::from(override_dir)
    } else {
        PathBuf::from(BASE_DIR)
    }
}

/// Reversibly encode a session id into a filesystem-safe stem using
/// lowercase hex. This preserves every byte of the raw id so
/// `scan_all()` can hand the TUI the exact same string the agent hook
/// stored, even when the raw id contains characters that would
/// otherwise require sanitization (UUIDs with colons, paths, CJK, …).
fn encode_session_id(session_id: &str) -> String {
    let mut out = String::with_capacity(session_id.len() * 2);
    for byte in session_id.as_bytes() {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// Inverse of [`encode_session_id`]. Returns `None` when the stem is
/// not valid UTF-8 hex of even length — such files are ignored by
/// `scan_all()` so stray artifacts never poison the name map.
fn decode_session_id(stem: &str) -> Option<String> {
    if stem.is_empty() || !stem.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(stem.len() / 2);
    let src = stem.as_bytes();
    for pair in src.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        bytes.push(((hi << 4) | lo) as u8);
    }
    String::from_utf8(bytes).ok()
}

pub fn name_path(session_id: &str) -> PathBuf {
    base_dir().join(format!("{}.txt", encode_session_id(session_id)))
}

pub fn read(session_id: &str) -> Option<String> {
    fs::read_to_string(name_path(session_id))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn write(session_id: &str, name: &str) -> io::Result<()> {
    let dir = base_dir();
    fs::create_dir_all(&dir)?;
    let final_path = name_path(session_id);
    let tmp_path = dir.join(format!(".{}.tmp", encode_session_id(session_id)));
    fs::write(&tmp_path, name)?;
    fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

pub fn scan_all() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let dir = base_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(raw_session_id) = decoded_session_id_from_path(&path) else {
            continue;
        };
        if let Ok(content) = fs::read_to_string(&path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                out.insert(raw_session_id, trimmed.to_string());
            }
        }
    }
    out
}

fn decoded_session_id_from_path(path: &Path) -> Option<String> {
    if path.extension().and_then(|e| e.to_str()) != Some("txt") {
        return None;
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.starts_with('.'))?;
    decode_session_id(stem)
}

pub fn latest_mtime() -> Option<SystemTime> {
    let dir = base_dir();
    let entries = fs::read_dir(&dir).ok()?;
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TempEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        dir: PathBuf,
    }

    impl TempEnv {
        fn new() -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!(
                "tmux-agent-sidebar-names-test-{}-{}",
                std::process::id(),
                rand_suffix()
            ));
            let _ = fs::remove_dir_all(&dir);
            // SAFETY: serialized via ENV_LOCK so no other thread is reading
            // or writing the env var while we mutate it.
            unsafe {
                std::env::set_var("TMUX_AGENT_SIDEBAR_NAMES_DIR", &dir);
            }
            TempEnv { _guard: guard, dir }
        }
    }

    impl Drop for TempEnv {
        fn drop(&mut self) {
            // SAFETY: still holding ENV_LOCK; no concurrent env access.
            unsafe {
                std::env::remove_var("TMUX_AGENT_SIDEBAR_NAMES_DIR");
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn rand_suffix() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{nanos:x}")
    }

    #[test]
    fn encode_is_reversible_for_plain_ids() {
        let encoded = encode_session_id("sess-1");
        assert_eq!(decode_session_id(&encoded).as_deref(), Some("sess-1"));
    }

    #[test]
    fn encode_is_reversible_for_ids_with_path_and_unicode() {
        for raw in [
            "../../etc/passwd",
            "ses/with/slash",
            "a b%c",
            "日本語-セッション",
        ] {
            let encoded = encode_session_id(raw);
            assert!(
                encoded.chars().all(|c| c.is_ascii_hexdigit()),
                "encoded form must only contain hex digits, got {encoded:?}"
            );
            assert_eq!(
                decode_session_id(&encoded).as_deref(),
                Some(raw),
                "round-trip must preserve the raw session id for {raw:?}"
            );
        }
    }

    #[test]
    fn decode_rejects_odd_length_and_non_hex() {
        assert!(decode_session_id("abc").is_none());
        assert!(decode_session_id("zz").is_none());
        assert!(decode_session_id("").is_none());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let _env = TempEnv::new();
        write("sess-1", "refactor").unwrap();
        assert_eq!(read("sess-1").as_deref(), Some("refactor"));
    }

    #[test]
    fn read_trims_whitespace_and_treats_empty_as_missing() {
        let _env = TempEnv::new();
        write("sess-empty", "   \n").unwrap();
        assert!(read("sess-empty").is_none());
    }

    #[test]
    fn read_returns_none_when_file_absent() {
        let _env = TempEnv::new();
        assert!(read("nope").is_none());
    }

    #[test]
    fn scan_all_returns_raw_session_ids_as_keys() {
        let _env = TempEnv::new();
        // Mix plain ASCII, a path-looking id, and a multibyte id to
        // prove the scan output is keyed by the ORIGINAL session_id
        // that the TUI later looks up with `pane.session_id`.
        write("sess-a", "alpha").unwrap();
        write("ses/with/slash", "slashy").unwrap();
        write("日本語-セッション", "ja").unwrap();
        // Writer leaves no tmp files normally, but simulate one sitting
        // around to prove scan_all ignores hidden/tmp artifacts.
        fs::create_dir_all(base_dir()).unwrap();
        fs::write(base_dir().join(".sess-c.tmp"), "gamma").unwrap();
        // A file whose stem is not valid hex must be skipped, not
        // surfaced as a bogus key.
        fs::write(base_dir().join("notes.txt"), "ignore-me").unwrap();

        let all = scan_all();
        assert_eq!(all.get("sess-a").map(String::as_str), Some("alpha"));
        assert_eq!(
            all.get("ses/with/slash").map(String::as_str),
            Some("slashy")
        );
        assert_eq!(all.get("日本語-セッション").map(String::as_str), Some("ja"));
        assert!(!all.contains_key("notes"));
        assert!(!all.contains_key(".sess-c"));
    }

    #[test]
    fn distinct_ids_that_were_previously_collision_prone_stay_distinct() {
        // Under the old sanitize-and-drop strategy these two ids both
        // collapsed to "ses_with_slash" and clobbered each other. Hex
        // encoding gives each id a unique filename.
        let _env = TempEnv::new();
        write("ses/with/slash", "first").unwrap();
        write("ses_with_slash", "second").unwrap();

        assert_eq!(read("ses/with/slash").as_deref(), Some("first"));
        assert_eq!(read("ses_with_slash").as_deref(), Some("second"));

        let all = scan_all();
        assert_eq!(all.get("ses/with/slash").map(String::as_str), Some("first"));
        assert_eq!(
            all.get("ses_with_slash").map(String::as_str),
            Some("second")
        );
    }

    #[test]
    fn scan_all_returns_empty_when_dir_missing() {
        let _env = TempEnv::new();
        let all = scan_all();
        assert!(all.is_empty());
    }

    #[test]
    fn path_traversal_in_session_id_is_contained() {
        let _env = TempEnv::new();
        // Hex-encoding never produces path separators, so an id that
        // looks like an escape attempt writes inside base_dir.
        let evil = "../../etc/passwd";
        write(evil, "x").unwrap();
        let written = name_path(evil);
        assert!(
            written.starts_with(base_dir()),
            "encoded path must stay under base_dir, got {written:?}"
        );
        assert_eq!(read(evil).as_deref(), Some("x"));
    }
}
