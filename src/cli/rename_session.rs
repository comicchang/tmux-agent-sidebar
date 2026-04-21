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

    // The lock serializes concurrent rename calls so a thundering herd
    // of simultaneous Stop events does not pin the local GPU. If the
    // lock cannot be acquired (perm error, /tmp weirdness), we log
    // explicitly and still proceed — the feature is best-effort and a
    // slightly concurrent call is better than a silently dropped one.
    let _lock = match acquire_lock() {
        Ok(file) => Some(file),
        Err(e) => {
            log_line(&format!(
                "rename-session: proceeding without flock ({e}); concurrent calls may overlap"
            ));
            None
        }
    };

    // Double-check after taking the lock so a concurrent call that just
    // wrote the name doesn't cause us to generate a duplicate.
    if parsed.auto && store::read(session_id).is_some() {
        return 0;
    }

    let pane = parsed
        .pane
        .clone()
        .or_else(|| find_pane_for_session(session_id));

    let user_payload = build_user_payload(pane.as_deref());

    match http::generate_name(&cfg, SYSTEM_PROMPT, &user_payload) {
        Ok(name) => {
            if !is_valid_title(&name) {
                log_line(&format!(
                    "rename-session: rejecting invalid title for {session_id}: {name:?}"
                ));
                return 1;
            }
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

/// Reject titles we should never render in the sidebar: empty strings,
/// strings containing ASCII control characters (which would break tmux
/// option storage and the row renderer), or strings that are pure
/// punctuation after stripping.
fn is_valid_title(title: &str) -> bool {
    if title.is_empty() {
        return false;
    }
    if title
        .chars()
        .any(|c| c.is_control() || c == '|' || c == '\t' || c == '\n' || c == '\r')
    {
        return false;
    }
    // Require at least one alphanumeric — a string of dashes or dots is
    // useless as a label.
    title.chars().any(|c| c.is_alphanumeric())
}

fn format_err(err: &LlmError) -> String {
    err.to_string()
}

fn build_user_payload(pane: Option<&str>) -> String {
    let mut buf = String::new();
    if let Some(pane_id) = pane {
        let entries = activity::read_activity_log(pane_id, ACTIVITY_LINES);
        if !entries.is_empty() {
            buf.push_str("Recent activity (newest first):\n");
            for e in entries {
                buf.push_str(&format!("- [{}] {} {}\n", e.timestamp, e.tool, e.label));
            }
        }
        // The Stop hook stores `last_assistant_message` in @pane_prompt
        // (see `handlers::on_stop`), so we read it from there instead
        // of accepting it via argv. argv would be visible to any other
        // user on the host via `ps`.
        let pane_prompt = tmux::get_pane_option_value(pane_id, "@pane_prompt");
        let msg = pane_prompt.trim();
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

fn acquire_lock() -> std::io::Result<File> {
    let dir = store::base_dir();
    std::fs::create_dir_all(&dir)?;
    let path: PathBuf = dir.join(".lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
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
        let args = vec!["--session=abc".into(), "--pane=%5".into(), "--auto".into()];
        let p = parse_args(&args);
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        assert_eq!(p.pane.as_deref(), Some("%5"));
        assert!(p.auto);
    }

    #[test]
    fn parse_args_space_form() {
        let args = vec![
            "--session".into(),
            "abc".into(),
            "--pane".into(),
            "%5".into(),
        ];
        let p = parse_args(&args);
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        assert_eq!(p.pane.as_deref(), Some("%5"));
        assert!(!p.auto);
    }

    #[test]
    fn parse_args_unknown_flags_ignored() {
        let args = vec!["--unknown".into(), "--session=x".into()];
        let p = parse_args(&args);
        assert_eq!(p.session_id.as_deref(), Some("x"));
    }

    /// Regression: last_message used to arrive via argv, exposing it to
    /// any other user on the host via `ps`. The flag has been removed
    /// — argv now only carries control metadata (session_id, pane_id,
    /// --auto). The last assistant message is read from `@pane_prompt`
    /// inside `build_user_payload` instead.
    #[test]
    fn parse_args_does_not_accept_last_message_flag() {
        let args = vec![
            "--session".into(),
            "abc".into(),
            "--last-message".into(),
            "secret".into(),
        ];
        let p = parse_args(&args);
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        // --last-message is treated as an unknown flag and its value
        // as a stray positional; neither should surface in ParsedArgs.
    }

    #[test]
    fn build_user_payload_fallback_when_no_pane() {
        let out = build_user_payload(None);
        assert!(out.contains("no activity"));
    }

    #[test]
    fn is_valid_title_accepts_well_formed_titles() {
        assert!(is_valid_title("refactor"));
        assert!(is_valid_title("fix-bug"));
        assert!(is_valid_title("日本語"));
        assert!(is_valid_title("api_v2"));
    }

    #[test]
    fn is_valid_title_rejects_empty_and_control_and_pipe() {
        assert!(!is_valid_title(""));
        assert!(!is_valid_title("has\nnewline"));
        assert!(!is_valid_title("has|pipe"));
        assert!(!is_valid_title("has\ttab"));
        assert!(!is_valid_title("bell\x07"));
    }

    #[test]
    fn is_valid_title_rejects_pure_punctuation() {
        assert!(!is_valid_title("---"));
        assert!(!is_valid_title("..."));
        assert!(!is_valid_title("-_-"));
    }
}
