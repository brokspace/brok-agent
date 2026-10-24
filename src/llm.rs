//! Minimal OpenAI-compatible chat completions client.
//!
//! Works with any server implementing `POST {base_url}/chat/completions`:
//! Ollama, LM Studio, vLLM, llama.cpp, Anthropic's and DeepSeek's
//! OpenAI-compatible endpoints, and OpenAI itself.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::{Stream, StreamExt, stream};
use serde::Serialize;
use serde_json::{Value, json};

use crate::settings::LlmProvider;

/// Errors raised while talking to the model.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// The provider references an API key variable that is not set.
    #[error("environment variable {0} is not set (it holds the API key for this provider)")]
    MissingApiKey(String),
    /// Transport failure.
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The server answered with a non-success status.
    #[error("provider returned HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Truncated response body.
        body: String,
    },
    /// The server reported an error inside the stream.
    #[error("provider error: {0}")]
    Api(String),
    /// The response could not be understood.
    #[error("unexpected response: {0}")]
    Decode(String),
}

/// Author of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Instructions.
    System,
    /// End user.
    User,
    /// The model (earlier turns of a conversation).
    Assistant,
}

/// One chat message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// Author.
    pub role: Role,
    /// Text.
    pub content: String,
}

impl Message {
    /// A system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: content.into() }
    }

    /// A user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into() }
    }

    /// An earlier answer of the model.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: content.into() }
    }
}

/// Client bound to one provider.
#[derive(Debug, Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
}

impl LlmClient {
    /// Builds a client, reading the API key from the environment if needed.
    pub fn new(provider: &LlmProvider) -> Result<Self, LlmError> {
        let api_key = match &provider.api_key_env {
            Some(var) => Some(std::env::var(var).map_err(|_| LlmError::MissingApiKey(var.clone()))?),
            None => None,
        };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("brok-agent/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            endpoint: format!("{}/chat/completions", provider.base_url.trim_end_matches('/')),
            model: provider.model.clone(),
            api_key,
            temperature: provider.temperature,
            max_tokens: provider.max_tokens,
        })
    }

    /// Model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    async fn send(&self, messages: &[Message], stream: bool) -> Result<reqwest::Response, LlmError> {
        let mut body = json!({ "model": self.model, "messages": messages, "stream": stream });
        if let Some(t) = self.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = self.max_tokens {
            body["max_tokens"] = json!(m);
        }
        let mut request = self.http.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        if !stream {
            request = request.timeout(Duration::from_secs(180));
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let mut body = response.text().await.unwrap_or_default();
            truncate(&mut body, 500);
            return Err(LlmError::Status { status: status.as_u16(), body });
        }
        Ok(response)
    }

    /// Non-streaming completion; returns the whole answer.
    pub async fn complete(&self, messages: &[Message]) -> Result<String, LlmError> {
        let value: Value = self.send(messages, false).await?.json().await?;
        if let Some(err) = api_error(&value) {
            return Err(LlmError::Api(err));
        }
        value["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| LlmError::Decode("missing choices[0].message.content".into()))
    }

    /// Streaming completion; yields text deltas as they arrive.
    pub async fn stream(
        &self,
        messages: &[Message],
    ) -> Result<impl Stream<Item = Result<String, LlmError>> + Send + 'static, LlmError> {
        let body = Box::pin(self.send(messages, true).await?.bytes_stream());
        let state = (body, SseParser::default(), VecDeque::<String>::new(), false);
        Ok(stream::unfold(state, |(mut body, mut parser, mut queue, mut done)| async move {
            loop {
                if let Some(data) = queue.pop_front() {
                    match parse_delta(&data) {
                        Ok(Delta::Done) => return None,
                        Ok(Delta::Text(text)) => return Some((Ok(text), (body, parser, queue, done))),
                        Ok(Delta::Empty) => continue,
                        Err(e) => return Some((Err(e), (body, parser, VecDeque::new(), true))),
                    }
                }
                if done {
                    return None;
                }
                match body.next().await {
                    Some(Ok(bytes)) => queue.extend(parser.push(&bytes)),
                    Some(Err(e)) => return Some((Err(e.into()), (body, parser, VecDeque::new(), true))),
                    None => {
                        queue.extend(parser.finish());
                        done = true;
                    }
                }
            }
        }))
    }
}

/// Incremental Server-Sent Events parser.
///
/// Feed raw bytes as they arrive; complete events are returned as their
/// (joined) `data` payloads. Chunk boundaries may fall anywhere, including
/// inside a multi-byte UTF-8 character or between `\r` and `\n`.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    /// Consumes bytes and returns the data of every completed event.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]);
            self.line(line.strip_suffix('\r').unwrap_or(&line), &mut events);
        }
        events
    }

    /// Flushes a trailing event not terminated by a blank line.
    pub fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            let line = String::from_utf8_lossy(&rest);
            self.line(line.strip_suffix('\r').unwrap_or(&line), &mut events);
        }
        self.line("", &mut events);
        events
    }

    fn line(&mut self, line: &str, events: &mut Vec<String>) {
        if line.is_empty() {
            if !self.data.is_empty() {
                events.push(self.data.join("\n"));
                self.data.clear();
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            self.data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
        }
        // Comments (`:`) and other fields (`event:`, `id:`, `retry:`) are ignored.
    }
}

/// Meaning of one SSE data payload.
#[derive(Debug, PartialEq, Eq)]
pub enum Delta {
    /// Text to append.
    Text(String),
    /// A chunk without content (role announcement, usage, ...).
    Empty,
    /// End of the stream.
    Done,
}

/// Interprets a chat completions stream payload.
pub fn parse_delta(data: &str) -> Result<Delta, LlmError> {
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(Delta::Done);
    }
    let value: Value = serde_json::from_str(data).map_err(|e| LlmError::Decode(format!("{e}: {data}")))?;
    if let Some(err) = api_error(&value) {
        return Err(LlmError::Api(err));
    }
    match value["choices"][0]["delta"]["content"].as_str() {
        Some(text) if !text.is_empty() => Ok(Delta::Text(text.to_owned())),
        _ => Ok(Delta::Empty),
    }
}

fn api_error(value: &Value) -> Option<String> {
    let err = value.get("error")?;
    Some(err.get("message").and_then(Value::as_str).map_or_else(|| err.to_string(), str::to_owned))
}

fn truncate(text: &mut String, max: usize) {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(text: &str) -> String {
        format!("data: {}\n\n", json!({ "choices": [{ "delta": { "content": text } }] }))
    }

    fn collect(parts: &[&[u8]]) -> Vec<String> {
        let mut parser = SseParser::default();
        let mut out: Vec<String> = parts.iter().flat_map(|p| parser.push(p)).collect();
        out.extend(parser.finish());
        out
    }

    #[test]
    fn parses_whole_events() {
        let raw = format!("{}{}data: [DONE]\n\n", chunk("Hel"), chunk("lo"));
        let events = collect(&[raw.as_bytes()]);
        assert_eq!(events.len(), 3);
        assert_eq!(parse_delta(&events[0]).unwrap(), Delta::Text("Hel".into()));
        assert_eq!(parse_delta(&events[2]).unwrap(), Delta::Done);
    }

    #[test]
    fn survives_arbitrary_chunk_boundaries() {
        let raw = format!("{}{}data: [DONE]\n\n", chunk("olá "), chunk("mundo ✓"));
        let bytes = raw.as_bytes();
        let expected = collect(&[bytes]);
        for split in 1..bytes.len() {
            assert_eq!(collect(&[&bytes[..split], &bytes[split..]]), expected, "split at {split}");
        }
        let single_bytes: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(collect(&single_bytes), expected);
    }

    #[test]
    fn handles_crlf_comments_and_fields() {
        let raw = b": keep-alive\r\nevent: message\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\r\n\r\n";
        let events = collect(&[raw]);
        assert_eq!(events.len(), 1);
        assert_eq!(parse_delta(&events[0]).unwrap(), Delta::Text("x".into()));
    }

    #[test]
    fn joins_multiline_data_and_flushes_tail() {
        assert_eq!(collect(&[b"data: a\ndata: b\n\ndata: tail"]), vec!["a\nb", "tail"]);
    }

    #[test]
    fn interprets_payloads() {
        assert_eq!(parse_delta(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#).unwrap(), Delta::Empty);
        assert_eq!(parse_delta(r#"{"choices":[]}"#).unwrap(), Delta::Empty);
        assert!(
            matches!(parse_delta(r#"{"error":{"message":"rate limited"}}"#), Err(LlmError::Api(m)) if m == "rate limited")
        );
        assert!(matches!(parse_delta("not json"), Err(LlmError::Decode(_))));
    }

    #[test]
    fn missing_api_key_is_explicit() {
        let provider = LlmProvider {
            base_url: "http://localhost:1/v1".into(),
            model: "m".into(),
            api_key_env: Some("BROK_AGENT_TEST_SURELY_UNSET_KEY".into()),
            temperature: None,
            max_tokens: None,
        };
        let err = LlmClient::new(&provider).unwrap_err();
        assert!(err.to_string().contains("BROK_AGENT_TEST_SURELY_UNSET_KEY"));
    }

    #[test]
    fn truncates_on_char_boundary() {
        let mut s = "ééé".to_string();
        truncate(&mut s, 3);
        assert_eq!(s, "é…");
    }
}
