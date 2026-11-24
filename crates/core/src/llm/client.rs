use std::collections::VecDeque;
use std::time::Duration;

use super::assemble::Assembler;
use super::sse::SseParser;
use super::types::{ChatRequest, LlmError, LlmEvent};
use super::wire::request_body;

/// Longest prefix of an error response body kept in `LlmError::Status`.
const ERROR_BODY_LIMIT: usize = 2048;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Endpoint root, for example `https://openrouter.ai/api/v1`; `/chat/completions` is appended.
    pub base_url: String,
    /// Sent as `Authorization: Bearer`; `None` sends no `Authorization` header.
    pub api_key: Option<String>,
    pub model: String,
    /// Send each assistant message's reasoning back as `reasoning_content`. `DeepSeek` requires it
    /// on requests carrying tools and answers 400 without it.
    pub send_reasoning: bool,
    pub connect_timeout: Duration,
    /// Longest wait for the next bytes of a response, headers included.
    pub read_timeout: Duration,
}

impl ClientConfig {
    #[must_use]
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            model: model.into(),
            send_reasoning: false,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    url: String,
    config: ClientConfig,
}

impl Client {
    pub fn new(config: ClientConfig) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .build()?;
        let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
        Ok(Self { http, url, config })
    }

    /// Starts one model call. Dropping the returned stream aborts the request.
    pub async fn stream(&self, request: &ChatRequest) -> Result<ChatStream, LlmError> {
        let body = request_body(&self.config.model, request, self.config.send_reasoning);
        let mut builder = self
            .http
            .post(&self.url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        if let Some(key) = &self.config.api_key {
            builder = builder.bearer_auth(key);
        }
        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::Status {
                status: status.as_u16(),
                body: text.chars().take(ERROR_BODY_LIMIT).collect(),
            });
        }
        Ok(ChatStream {
            response,
            parser: SseParser::default(),
            assembler: Assembler::default(),
            pending: VecDeque::new(),
            error: None,
            finished: false,
        })
    }
}

/// The events of one model call, in order. Ends after `LlmEvent::Done` or after the first error.
#[derive(Debug)]
pub struct ChatStream {
    response: reqwest::Response,
    parser: SseParser,
    assembler: Assembler,
    pending: VecDeque<LlmEvent>,
    /// Returned once every event decoded before it has been handed out.
    error: Option<LlmError>,
    finished: bool,
}

impl ChatStream {
    pub async fn next(&mut self) -> Option<Result<LlmEvent, LlmError>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(Ok(event));
            }
            if let Some(error) = self.error.take() {
                return Some(Err(error));
            }
            if self.finished {
                return None;
            }
            if let Err(error) = self.read().await {
                self.finished = true;
                self.error = Some(error);
            }
        }
    }

    async fn read(&mut self) -> Result<(), LlmError> {
        if let Some(bytes) = self.response.chunk().await? {
            for data in self.parser.push(&bytes) {
                self.apply(&data)?;
                if self.finished {
                    break;
                }
            }
            return Ok(());
        }
        if let Some(data) = self.parser.finish() {
            self.apply(&data)?;
        }
        if !self.finished {
            let completion = self.assembler.finish_without_done()?;
            self.pending.push_back(LlmEvent::Done(completion));
            self.finished = true;
        }
        Ok(())
    }

    fn apply(&mut self, data: &str) -> Result<(), LlmError> {
        for event in self.assembler.apply(data)? {
            self.finished |= matches!(event, LlmEvent::Done(_));
            self.pending.push_back(event);
        }
        Ok(())
    }
}
