use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use super::client::{self, ChatRequest, LlmError};
use super::config::LlmConfig;

struct ParsedEndpoint<'a> {
    host: &'a str,
    port: u16,
    path: String,
}

fn parse_endpoint(endpoint: &str) -> Result<ParsedEndpoint<'_>, LlmError> {
    let rest = endpoint.strip_prefix("http://").ok_or_else(|| {
        LlmError::Http(format!(
            "unsupported scheme in {endpoint:?} (http:// only in v1)"
        ))
    })?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p
                .parse()
                .map_err(|_| LlmError::Http(format!("bad port in {endpoint:?}")))?;
            (h, port)
        }
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err(LlmError::Http(format!("empty host in {endpoint:?}")));
    }
    Ok(ParsedEndpoint { host, port, path })
}

fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, LlmError> {
    let mut last_err: Option<std::io::Error> = None;
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| LlmError::Http(format!("resolve {host}:{port}: {e}")))?;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(LlmError::Http(match last_err {
        Some(e) => format!("connect {host}:{port}: {e}"),
        None => format!("no addresses resolved for {host}:{port}"),
    }))
}

/// Return `true` when `host` is guaranteed to resolve only to loopback
/// addresses. Accepts the literal IPs `127.0.0.0/8` and `::1`, plus
/// the conventional hostname `localhost`. Any other hostname is
/// rejected so a misconfigured `@sidebar_llm_endpoint` cannot exfil
/// a bearer token over plaintext to a remote host.
fn host_is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// Perform a plaintext HTTP/1.1 POST and return the response body.
///
/// v1 only speaks `http://` to cover local runners (Ollama, LM Studio,
/// llama.cpp server) without pulling in a TLS stack. When `api_key`
/// is provided, the target host MUST be a loopback address so the
/// `Authorization: Bearer …` header is never sent over an
/// unencrypted off-host wire.
pub fn post(
    endpoint: &str,
    body: &str,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<String, LlmError> {
    let parsed = parse_endpoint(endpoint)?;
    if api_key.is_some() && !host_is_loopback(parsed.host) {
        return Err(LlmError::Http(format!(
            "refusing to send Authorization over plaintext http:// to non-loopback host {:?}",
            parsed.host
        )));
    }
    let mut stream = connect(parsed.host, parsed.port, timeout)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| LlmError::Http(format!("set_read_timeout: {e}")))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| LlmError::Http(format!("set_write_timeout: {e}")))?;

    let host_header = if parsed.port == 80 {
        parsed.host.to_string()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let mut request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: tmux-agent-sidebar\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n",
        path = parsed.path,
        host = host_header,
        len = body.len()
    );
    if let Some(key) = api_key {
        request.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    request.push_str("\r\n");

    stream
        .write_all(request.as_bytes())
        .map_err(|e| LlmError::Http(format!("write headers: {e}")))?;
    stream
        .write_all(body.as_bytes())
        .map_err(|e| LlmError::Http(format!("write body: {e}")))?;
    stream
        .flush()
        .map_err(|e| LlmError::Http(format!("flush: {e}")))?;

    let mut reader = BufReader::new(stream);

    let status_line = read_line(&mut reader)?;
    let (status_code, _reason) = parse_status_line(&status_line)?;

    let mut content_length: Option<usize> = None;
    let mut transfer_encoding: Option<String> = None;
    loop {
        let line = read_line(&mut reader)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            match name.as_str() {
                "content-length" => {
                    content_length = value.parse().ok();
                }
                "transfer-encoding" => {
                    transfer_encoding = Some(value.to_ascii_lowercase());
                }
                _ => {}
            }
        }
    }

    let body_bytes = if transfer_encoding.as_deref() == Some("chunked") {
        read_chunked(&mut reader)?
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| LlmError::Http(format!("read body ({len} bytes): {e}")))?;
        buf
    } else {
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .map_err(|e| LlmError::Http(format!("read body: {e}")))?;
        buf
    };

    if !(200..300).contains(&status_code) {
        let snippet = String::from_utf8_lossy(&body_bytes);
        return Err(LlmError::Http(format!(
            "http {status_code}: {}",
            snippet.chars().take(200).collect::<String>()
        )));
    }

    String::from_utf8(body_bytes).map_err(|e| LlmError::Http(format!("utf8: {e}")))
}

fn read_line<R: BufRead>(reader: &mut R) -> Result<String, LlmError> {
    let mut buf = String::new();
    reader
        .read_line(&mut buf)
        .map_err(|e| LlmError::Http(format!("read line: {e}")))?;
    Ok(buf)
}

fn parse_status_line(line: &str) -> Result<(u16, String), LlmError> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let mut parts = trimmed.splitn(3, ' ');
    let _version = parts
        .next()
        .ok_or_else(|| LlmError::Http(format!("empty status line: {trimmed:?}")))?;
    let code_str = parts
        .next()
        .ok_or_else(|| LlmError::Http(format!("no status code in: {trimmed:?}")))?;
    let code: u16 = code_str
        .parse()
        .map_err(|_| LlmError::Http(format!("bad status code: {code_str:?}")))?;
    let reason = parts.next().unwrap_or("").to_string();
    Ok((code, reason))
}

fn read_chunked<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, LlmError> {
    let mut out = Vec::new();
    loop {
        let size_line = read_line(reader)?;
        let size_str = size_line
            .trim_end_matches(['\r', '\n'])
            .split(';')
            .next()
            .unwrap_or("");
        let size = usize::from_str_radix(size_str.trim(), 16)
            .map_err(|_| LlmError::Http(format!("bad chunk size: {size_str:?}")))?;
        if size == 0 {
            let _trailer = read_line(reader)?;
            break;
        }
        let mut chunk = vec![0u8; size];
        reader
            .read_exact(&mut chunk)
            .map_err(|e| LlmError::Http(format!("read chunk: {e}")))?;
        out.extend_from_slice(&chunk);
        let _crlf = read_line(reader)?;
    }
    Ok(out)
}

/// End-to-end: build request, send it, parse response, return cleaned title.
pub fn generate_name(cfg: &LlmConfig, system: &str, user: &str) -> Result<String, LlmError> {
    let body = client::build_body(&ChatRequest {
        model: &cfg.model,
        system,
        user,
    });
    let response = post(
        &cfg.endpoint,
        &body,
        cfg.api_key.as_deref(),
        Duration::from_millis(cfg.timeout_ms),
    )?;
    client::parse_response(&response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn echo_server(response_body: &'static str) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://{addr}/v1/chat/completions");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4) {
                    let header_str = std::str::from_utf8(&buf[..idx - 4]).unwrap_or("");
                    let content_length = header_str
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length: ")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if buf.len() - idx >= content_length {
                        break;
                    }
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                len = response_body.len(),
                body = response_body
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
            buf
        });
        (endpoint, handle)
    }

    #[test]
    fn parse_endpoint_extracts_host_port_path() {
        let p = parse_endpoint("http://localhost:11434/v1/chat/completions").unwrap();
        assert_eq!(p.host, "localhost");
        assert_eq!(p.port, 11434);
        assert_eq!(p.path, "/v1/chat/completions");
    }

    #[test]
    fn parse_endpoint_defaults_port_80_and_root_path() {
        let p = parse_endpoint("http://example").unwrap();
        assert_eq!(p.host, "example");
        assert_eq!(p.port, 80);
        assert_eq!(p.path, "/");
    }

    #[test]
    fn parse_endpoint_rejects_https() {
        assert!(matches!(
            parse_endpoint("https://example/v1/chat/completions"),
            Err(LlmError::Http(_))
        ));
    }

    #[test]
    fn parse_endpoint_rejects_missing_scheme() {
        assert!(matches!(
            parse_endpoint("localhost:11434"),
            Err(LlmError::Http(_))
        ));
    }

    #[test]
    fn generate_name_full_roundtrip_against_local_server() {
        let (endpoint, handle) = echo_server(r#"{"choices":[{"message":{"content":"refactor"}}]}"#);
        let cfg = LlmConfig {
            endpoint,
            model: "test-model".into(),
            api_key: None,
            auto_rename: false,
            timeout_ms: 5_000,
        };
        let name = generate_name(&cfg, "system prompt", "user log").unwrap();
        assert_eq!(name, "refactor");

        let request_bytes = handle.join().unwrap();
        let request = String::from_utf8_lossy(&request_bytes);
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(request.contains("Content-Type: application/json"));
        assert!(request.contains("\"model\":\"test-model\""));
        assert!(request.contains("\"content\":\"system prompt\""));
        assert!(request.contains("\"content\":\"user log\""));
        // No Authorization header when api_key is None.
        assert!(
            !request.contains("Authorization:"),
            "expected no Authorization header, got: {request}"
        );
    }

    #[test]
    fn generate_name_includes_authorization_when_api_key_set() {
        let (endpoint, handle) = echo_server(r#"{"choices":[{"message":{"content":"deploy"}}]}"#);
        let cfg = LlmConfig {
            endpoint,
            model: "m".into(),
            api_key: Some("sk-secret".into()),
            auto_rename: false,
            timeout_ms: 5_000,
        };
        let name = generate_name(&cfg, "s", "u").unwrap();
        assert_eq!(name, "deploy");

        let request = String::from_utf8_lossy(&handle.join().unwrap()).to_string();
        assert!(request.contains("Authorization: Bearer sk-secret"));
    }

    #[test]
    fn host_is_loopback_accepts_localhost_and_127_and_v6_loopback() {
        assert!(host_is_loopback("localhost"));
        assert!(host_is_loopback("LOCALHOST"));
        assert!(host_is_loopback("127.0.0.1"));
        assert!(host_is_loopback("127.1.2.3"));
        assert!(host_is_loopback("::1"));
    }

    #[test]
    fn host_is_loopback_rejects_remote_hosts_and_dns_names() {
        assert!(!host_is_loopback("example.com"));
        assert!(!host_is_loopback("api.openai.com"));
        assert!(!host_is_loopback("192.168.1.10"));
        assert!(!host_is_loopback("10.0.0.1"));
        // A DNS name that might resolve to loopback is still rejected
        // — we refuse to depend on runtime resolution for a trust call.
        assert!(!host_is_loopback("my-host.local"));
    }

    #[test]
    fn post_refuses_bearer_over_plaintext_to_remote_host() {
        // No server needs to exist — the guard short-circuits before
        // we try to connect.
        let err = post(
            "http://api.example.com/v1/chat/completions",
            "{}",
            Some("sk-secret"),
            Duration::from_millis(500),
        )
        .unwrap_err();
        match err {
            LlmError::Http(msg) => {
                assert!(
                    msg.contains("Authorization") && msg.contains("non-loopback"),
                    "expected credential guard error, got: {msg}"
                );
            }
            other => panic!("expected Http guard error, got {other:?}"),
        }
    }

    #[test]
    fn post_allows_bearer_over_loopback() {
        // Loopback + api_key must not be short-circuited. We verify by
        // running a real roundtrip against a 127.0.0.1 listener.
        let (endpoint, handle) = echo_server(r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        let body = post(
            &endpoint,
            "{}",
            Some("sk-local"),
            Duration::from_millis(2_000),
        )
        .unwrap();
        assert!(body.contains("\"content\":\"ok\""));
        let request = String::from_utf8_lossy(&handle.join().unwrap()).to_string();
        assert!(request.contains("Authorization: Bearer sk-local"));
    }

    #[test]
    fn post_propagates_non_2xx_as_http_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://{addr}/");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let resp = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\noops!";
            stream.write_all(resp.as_bytes()).unwrap();
        });
        let err = post(&endpoint, "{}", None, Duration::from_millis(2_000)).unwrap_err();
        server.join().unwrap();
        match err {
            LlmError::Http(msg) => {
                assert!(msg.contains("500"), "expected 500 in error, got: {msg}");
            }
            other => panic!("expected Http error, got {other:?}"),
        }
    }
}
