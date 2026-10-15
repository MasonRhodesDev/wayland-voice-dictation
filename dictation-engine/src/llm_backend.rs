//! Client for a local OpenAI-compatible chat endpoint (llama.cpp server,
//! Ollama, vLLM, LM Studio).
//!
//! `[llm_backend]` holds only where the model lives. Features that use it
//! (the `llm_format` stage, command mode) each pass their own timeout, so a
//! formatting pass can stay tight while a longer rewrite waits longer.
//!
//! Every call runs on its own thread and the caller stops waiting at the
//! deadline, even when the socket is still open.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// `[llm_backend]` config section. Also accepted as an inline table:
/// `llm_backend = { url = "http://127.0.0.1:8080", model = "qwen2.5-3b" }`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LlmBackendConfig {
    /// Server base URL, e.g. `http://127.0.0.1:8080` (llama.cpp) or
    /// `http://127.0.0.1:11434` (Ollama). A URL that already ends in
    /// `/chat/completions` is used as is.
    #[serde(default)]
    pub url: String,
    /// Model name sent with each request. llama.cpp ignores it; Ollama needs it.
    #[serde(default)]
    pub model: String,
    /// Optional bearer token. Empty sends no `Authorization` header.
    #[serde(default)]
    pub api_key: String,
}

impl LlmBackendConfig {
    pub fn is_configured(&self) -> bool {
        !self.url.trim().is_empty() && !self.model.trim().is_empty()
    }

    /// Full chat-completions URL for the configured base.
    pub fn endpoint(&self) -> String {
        let base = self.url.trim().trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }
}

/// One chat request: a system prompt and a single user turn.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub system: String,
    pub user: String,
    pub max_tokens: usize,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct LlmClient {
    config: LlmBackendConfig,
}

impl LlmClient {
    pub fn new(config: LlmBackendConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &LlmBackendConfig {
        &self.config
    }

    /// Send one request and return the assistant text, or an error on
    /// timeout, transport failure, a non-2xx status or an unexpected body.
    /// Never blocks longer than `req.timeout`.
    pub fn chat(&self, req: &ChatRequest) -> Result<String> {
        if !self.config.is_configured() {
            bail!("[llm_backend] url and model are not set");
        }
        let timeout = req.timeout.max(Duration::from_millis(50));
        let (tx, rx) = mpsc::channel();
        let config = self.config.clone();
        let req_owned = req.clone();
        std::thread::Builder::new()
            .name("llm-backend".into())
            .spawn(move || {
                let _ = tx.send(post_chat(&config, &req_owned, timeout));
            })
            .context("spawning llm-backend thread")?;
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(_) => Err(anyhow!("timed out after {} ms", timeout.as_millis())),
        }
    }
}

fn post_chat(config: &LlmBackendConfig, req: &ChatRequest, timeout: Duration) -> Result<String> {
    let body = serde_json::json!({
        "model": config.model.trim(),
        "messages": [
            { "role": "system", "content": req.system },
            { "role": "user", "content": req.user },
        ],
        "temperature": 0,
        "max_tokens": req.max_tokens,
        "stream": false,
    });
    let client = reqwest::blocking::Client::builder().timeout(timeout).build()?;
    let mut http = client
        .post(config.endpoint())
        .header("content-type", "application/json")
        .body(body.to_string());
    if !config.api_key.trim().is_empty() {
        http = http.bearer_auth(config.api_key.trim());
    }
    let resp = http.send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        bail!("HTTP {status}: {}", truncate(&text, 300));
    }
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("invalid chat response: {}", truncate(&text, 300)))?;
    let content = parsed["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow!("chat response has no message content: {}", truncate(&text, 300)))?;
    Ok(strip_reasoning(content).to_string())
}

/// Drop a leading `<think>...</think>` block that reasoning models emit.
fn strip_reasoning(s: &str) -> &str {
    let t = s.trim_start();
    match t.strip_prefix("<think>").and_then(|rest| rest.split_once("</think>")) {
        Some((_, after)) => after,
        None => s,
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Minimal HTTP server for tests: answers every request with a canned
/// status and body, optionally after a delay. No network beyond loopback.
#[cfg(test)]
pub(crate) mod mock {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub struct MockServer {
        pub url: String,
        /// Request bodies received, in order.
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    /// Serve `status` with `body` for every connection, after `delay`.
    pub fn serve(status: u16, body: String, delay: Duration) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let req = read_request(&mut stream);
                seen.lock().unwrap().push(req);
                std::thread::sleep(delay);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        MockServer { url, requests }
    }

    /// Serve an OpenAI-style chat completion whose content is `content`.
    pub fn serve_reply(content: &str) -> MockServer {
        let body = serde_json::json!({
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": content } }]
        })
        .to_string();
        serve(200, body, Duration::ZERO)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text[..end]
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
                    })
                    .unwrap_or(0usize);
                if buf.len() >= end + 4 + len {
                    return String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).into_owned();
                }
            }
        }
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::mock::*;
    use super::*;

    fn client(url: &str) -> LlmClient {
        LlmClient::new(LlmBackendConfig {
            url: url.into(),
            model: "test-model".into(),
            api_key: String::new(),
        })
    }

    fn request(timeout_ms: u64) -> ChatRequest {
        ChatRequest {
            system: "sys".into(),
            user: "hello".into(),
            max_tokens: 64,
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    #[test]
    fn endpoint_normalisation() {
        let mut c = LlmBackendConfig { url: "http://h:8080".into(), ..Default::default() };
        assert_eq!(c.endpoint(), "http://h:8080/v1/chat/completions");
        c.url = "http://h:11434/v1/".into();
        assert_eq!(c.endpoint(), "http://h:11434/v1/chat/completions");
        c.url = "http://h/custom/chat/completions".into();
        assert_eq!(c.endpoint(), "http://h/custom/chat/completions");
    }

    #[test]
    fn unconfigured_backend() {
        assert!(!LlmBackendConfig::default().is_configured());
        let c = LlmBackendConfig { url: "http://h".into(), ..Default::default() };
        assert!(!c.is_configured());
        assert!(LlmClient::new(c).chat(&request(500)).is_err());
    }

    #[test]
    fn returns_content_and_sends_model_and_messages() {
        let server = serve_reply("Hello.");
        assert_eq!(client(&server.url).chat(&request(2000)).unwrap(), "Hello.");
        let body: serde_json::Value =
            serde_json::from_str(&server.requests.lock().unwrap()[0]).unwrap();
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hello");
        assert_eq!(body["temperature"], 0);
    }

    #[test]
    fn strips_think_block() {
        let server = serve_reply("<think>\nhmm\n</think>\nHello.");
        assert_eq!(client(&server.url).chat(&request(2000)).unwrap().trim(), "Hello.");
    }

    #[test]
    fn http_error_is_an_error() {
        let server = serve(500, "{\"error\":\"boom\"}".into(), Duration::ZERO);
        let err = client(&server.url).chat(&request(2000)).unwrap_err();
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[test]
    fn malformed_body_is_an_error() {
        let server = serve(200, "{\"choices\":[]}".into(), Duration::ZERO);
        assert!(client(&server.url).chat(&request(2000)).is_err());
    }

    #[test]
    fn slow_server_times_out_at_the_deadline() {
        let server = serve(200, "{}".into(), Duration::from_millis(1500));
        let started = std::time::Instant::now();
        let err = client(&server.url).chat(&request(200)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_millis(1000));
    }

    #[test]
    fn inline_table_config_parses() {
        #[derive(Deserialize)]
        struct Wrap {
            llm_backend: LlmBackendConfig,
        }
        let w: Wrap =
            toml::from_str("llm_backend = { url = \"http://127.0.0.1:8080\", model = \"m\" }")
                .unwrap();
        assert!(w.llm_backend.is_configured());
    }
}
