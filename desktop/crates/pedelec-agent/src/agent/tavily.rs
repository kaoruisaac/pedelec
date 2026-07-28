use super::error::AgentError;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use std::time::Duration;

const TAVILY_SEARCH_ENDPOINT: &str = "https://api.tavily.com/search";
const TAVILY_REQUEST_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TAVILY_SEARCHES_PER_MODEL_ROUND: usize = 10;

pub struct TavilyClient {
    api_key: String,
    client: Client,
}

impl TavilyClient {
    pub fn new(api_key: String) -> Result<Self, AgentError> {
        let client = Client::builder()
            .timeout(Duration::from_millis(TAVILY_REQUEST_TIMEOUT_MS))
            .build()
            .map_err(|_| {
                AgentError::new("TAVILY_UNAVAILABLE", "Unable to initialize web search.")
            })?;
        Ok(Self { api_key, client })
    }

    fn search(&self, query: &str) -> Result<Value, AgentError> {
        let body = serde_json::json!({
            "query": query,
            "search_depth": "basic",
            "chunks_per_source": 1,
            "max_results": 5,
            "topic": "general",
            "include_answer": false,
            "include_raw_content": false,
            "include_images": false,
            "include_image_descriptions": false,
            "include_favicon": false,
            "auto_parameters": false,
        });
        let response = self
            .client
            .post(TAVILY_SEARCH_ENDPOINT)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .map_err(|_| {
                AgentError::new("TAVILY_UNAVAILABLE", "Web search is currently unavailable.")
            })?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let (code, message) = match status {
                400 => ("TAVILY_REQUEST_INVALID", "Web search request was invalid."),
                401 | 403 => (
                    "TAVILY_AUTH_FAILED",
                    "Web search authentication failed. Check the Tavily API key.",
                ),
                429 => (
                    "TAVILY_RATE_LIMITED",
                    "Web search is rate limited. Try again later.",
                ),
                432 | 433 => (
                    "TAVILY_USAGE_LIMIT_EXCEEDED",
                    "Web search usage limit has been reached.",
                ),
                _ => ("TAVILY_REQUEST_FAILED", "Web search request failed."),
            };
            return Err(AgentError::new(code, message));
        }
        let value: Value = response.json().map_err(|_| {
            AgentError::new(
                "TAVILY_RESPONSE_INVALID",
                "Web search returned an invalid response.",
            )
        })?;
        let results = value
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AgentError::new(
                    "TAVILY_RESPONSE_INVALID",
                    "Web search response had an invalid results field.",
                )
            })?;
        let mut compact = Vec::new();
        for result in results.iter().take(5) {
            let title = result.get("title").and_then(Value::as_str).ok_or_else(|| {
                AgentError::new(
                    "TAVILY_RESPONSE_INVALID",
                    "Web search result had an invalid title.",
                )
            })?;
            let url = result.get("url").and_then(Value::as_str).ok_or_else(|| {
                AgentError::new(
                    "TAVILY_RESPONSE_INVALID",
                    "Web search result had an invalid URL.",
                )
            })?;
            let content = result
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    AgentError::new(
                        "TAVILY_RESPONSE_INVALID",
                        "Web search result had invalid content.",
                    )
                })?;
            let mut item = serde_json::json!({"title": title, "url": url, "content": content});
            if let Some(score) = result.get("score").and_then(Value::as_f64) {
                item["score"] = serde_json::json!(score);
            }
            compact.push(item);
        }
        Ok(serde_json::json!({"query": query, "results": compact}))
    }
}

pub struct TavilyRoundWrapper<'a> {
    client: &'a TavilyClient,
    attempted_searches: usize,
}

impl<'a> TavilyRoundWrapper<'a> {
    pub fn new(client: &'a TavilyClient) -> Self {
        Self {
            client,
            attempted_searches: 0,
        }
    }

    pub fn search(&mut self, query: &str) -> Result<Value, AgentError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(AgentError::new(
                "INVALID_ARGUMENT",
                "web.search requires a non-empty query.",
            ));
        }
        if self.attempted_searches >= MAX_TAVILY_SEARCHES_PER_MODEL_ROUND {
            return Err(AgentError::new(
                "TAVILY_MODEL_ROUND_LIMIT_EXCEEDED",
                "Web search is limited to 10 calls per model round.",
            ));
        }
        self.attempted_searches += 1;
        self.client.search(query)
    }
}
