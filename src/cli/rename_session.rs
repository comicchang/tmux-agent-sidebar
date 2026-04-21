use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use crate::activity;
use crate::llm::{client::LlmError, config::LlmConfig, http, store};
use crate::tmux;

const LOG_PATH: &str = "/tmp/tmux-agent-sidebar-llm.log";
const SYSTEM_PROMPT: &str = "You are naming a coding session. Read the log and return ONE SHORT WORD (max 16 characters, no whitespace) that captures the topic. Match the user's language. Output ONLY the word — no quotes, no punctuation, no explanation.";
const USER_MESSAGE_CAP: usize = 800;
const ACTIVITY_LINES: usize = 20;

#[derive(Debug, Default)]
struct ParsedArgs {
    session_id: Option<String>,
    pane: Option<String>,
    last_message: Option<String>,
    auto: bool,
}

fn parse_args(args: &[String]) -> ParsedArgs {
    let mut out = ParsedArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(value) = arg.strip_prefix("--session=") {
            out.session_id = Some(value.to_string());
        } else if arg == "--session" {
            i += 1;
            if let Some(v) = args.get(i) {
                out.session_id = Some(v.clone());
            }
        } else if let Some(value) = arg.strip_prefix("--pane=") {
            out.pane = Some(value.to_string());
        } else if arg == "--pane" {
            i += 1;
            if let Some(v) = args.get(i) {
                out.pane = Some(v.clone());
            }
        } else if let Some(value) = arg.strip_prefix("--last-message=") {
            out.last_message = Some(value.to_string());
        } else if arg == "--last-message" {
            i += 1;
            if let Some(v) = args.get(i) {
                out.last_message = Some(v.clone());
            }
        } else if arg == "--auto" {
            out.auto = true;
        }
        i += 1;
    }
    out
}

pub fn cmd_rename_session(args: &[String]) -> i32 {
    let parsed = parse_args(args);
    let Some(session_id) = parsed.session_id.as_deref() else {
        log_line("rename-session: missing --session");
        return 2;
    };
    if session_id.is_empty() {
        return 0;
    }

    if parsed.auto && store::read(session_id).is_some() {
        return 0;
    }

    let opts = tmux::get_all_global_options();
    let Some(cfg) = LlmConfig::from_tmux_options(&opts) else {
        // Feature not configured — treat as a no-op.
        return 0;
    };

    let _lock = acquire_lock();

    // Double-check after taking the lock so a concurrent call that just
    // wrote the name doesn't cause us to generate a duplicate.
    if parsed.auto && store::read(session_id).is_some() {
        return 0;
    }

    let pane = parsed
        .pane
        .clone()
        .or_else(|| find_pane_for_session(session_id));

    let user_payload = build_user_payload(pane.as_deref(), parsed.last_message.as_deref());

    match http::generate_name(&cfg, SYSTEM_PROMPT, &user_payload) {
        Ok(name) => {
            if let Err(e) = store::write(session_id, &name) {
                log_line(&format!(
                    "rename-session: write failed for {session_id}: {e}"
                ));
                return 1;
            }
            println!("{name}");
            0
        }
        Err(e) => {
            log_line(&format!(
                "rename-session: llm call failed for {session_id}: {}",
                format_err(&e)
            ));
            1
        }
    }
}

fn format_err(err: &LlmError) -> String {
    err.to_string()
}

fn build_user_payload(pane: Option<&str>, last_message: Option<&str>) -> String {
    let mut buf = String::new();
    if let Some(pane_id) = pane {
        let entries = activity::read_activity_log(pane_id, ACTIVITY_LINES);
        if !entries.is_empty() {
            buf.push_str("Recent activity (newest first):\n");
            for e in entries {
                buf.push_str(&format!("- [{}] {} {}\n", e.timestamp, e.tool, e.label));
            }
        }
    }
    if let Some(msg) = last_message {
        let msg = msg.trim();
        if !msg.is_empty() {
            let truncated: String = msg.chars().take(USER_MESSAGE_CAP).collect();
            buf.push_str("\nLast assistant message:\n");
            buf.push_str(&truncated);
        }
    }
    if buf.is_empty() {
        buf.push_str("(no activity recorded yet)");
    }
    buf
}

fn find_pane_for_session(session_id: &str) -> Option<String> {
    for session in tmux::query_sessions() {
        for window in session.windows {
            for pane in window.panes {
                if pane.session_id.as_deref() == Some(session_id) {
                    return Some(pane.pane_id);
                }
            }
        }
    }
    None
}

fn acquire_lock() -> Option<File> {
    let dir = store::base_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path: PathBuf = dir.join(".lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    unsafe {
        if libc::flock(file.as_raw_fd(), libc::LOCK_EX) != 0 {
            return None;
        }
    }
    Some(file)
}

fn log_line(msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        let _ = writeln!(
            f,
            "[{}] {msg}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
    }
    eprintln!("{msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_equals_form() {
        let args = vec![
            "--session=abc".into(),
            "--pane=%5".into(),
            "--last-message=done".into(),
            "--auto".into(),
        ];
        let p = parse_args(&args);
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        assert_eq!(p.pane.as_deref(), Some("%5"));
        assert_eq!(p.last_message.as_deref(), Some("done"));
        assert!(p.auto);
    }

    #[test]
    fn parse_args_space_form() {
        let args = vec![
            "--session".into(),
            "abc".into(),
            "--pane".into(),
            "%5".into(),
            "--last-message".into(),
            "done".into(),
        ];
        let p = parse_args(&args);
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        assert_eq!(p.pane.as_deref(), Some("%5"));
        assert_eq!(p.last_message.as_deref(), Some("done"));
        assert!(!p.auto);
    }

    #[test]
    fn parse_args_unknown_flags_ignored() {
        let args = vec!["--unknown".into(), "--session=x".into()];
        let p = parse_args(&args);
        assert_eq!(p.session_id.as_deref(), Some("x"));
    }

    #[test]
    fn build_user_payload_includes_last_message_when_no_pane() {
        let out = build_user_payload(None, Some("done refactoring"));
        assert!(out.contains("Last assistant message"));
        assert!(out.contains("done refactoring"));
    }

    #[test]
    fn build_user_payload_truncates_long_last_message() {
        let long = "a".repeat(2_000);
        let out = build_user_payload(None, Some(&long));
        assert!(out.contains(&"a".repeat(USER_MESSAGE_CAP)));
        assert!(!out.contains(&"a".repeat(USER_MESSAGE_CAP + 1)));
    }

    #[test]
    fn build_user_payload_fallback_when_all_empty() {
        let out = build_user_payload(None, None);
        assert!(out.contains("no activity"));
    }
}
