//! LLM-backed tool summarizer with caching.
//!
//! When the rule-based `summarize::summarize_tool_action()` returns a generic
//! summary (e.g. "Using Foo"), this module optionally calls an external LLM
//! endpoint to produce a more descriptive one-liner. Results are cached
//! in-memory (capped at 200 entries) to avoid repeated API calls.
//!
//! Supported backends:
//! - Ollama (`/api/chat` or `/api/generate`)
//! - Anthropic (`/v1/messages`)
//! - Generic chat (`{"prompt": ...}` -> `{"response": ...}`)

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// LLM-backed summarizer with in-memory cache.
pub struct LlmSummarizer {
    client: reqwest::Client,
    endpoint: String,
    api_key: Option<String>,
    enabled: bool,
    cache: Arc<RwLock<HashMap<String, String>>>,
}

const MAX_CACHE: usize = 200;

impl LlmSummarizer {
    /// Create a new summarizer. Disabled if `endpoint` is `None`.
    pub fn new(endpoint: Option<String>, api_key: Option<String>) -> Self {
        let enabled = endpoint.is_some();
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            endpoint: endpoint.unwrap_or_default(),
            api_key,
            enabled,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Summarize a tool action via LLM if the rule-based summary is generic.
    ///
    /// Returns the rule-based summary as-is when it's already specific,
    /// or when the LLM endpoint is unavailable/disabled.
    pub async fn summarize(
        &self,
        tool: &str,
        input: &serde_json::Value,
        rule_based: &str,
    ) -> String {
        if !self.enabled || !is_generic_summary(rule_based) {
            return rule_based.to_string();
        }

        let cache_key = build_cache_key(tool, input);

        // Check cache first
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(&cache_key) {
                return cached.clone();
            }
        }

        // Call LLM
        match self.call_llm(tool, input).await {
            Ok(summary) if !summary.is_empty() => {
                let mut cache = self.cache.write().await;
                if cache.len() >= MAX_CACHE {
                    cache.clear();
                }
                cache.insert(cache_key, summary.clone());
                summary
            }
            Err(e) => {
                tracing::debug!(error = %e, "LLM summarizer failed, using rule-based");
                rule_based.to_string()
            }
            _ => rule_based.to_string(),
        }
    }

    async fn call_llm(&self, tool: &str, input: &serde_json::Value) -> anyhow::Result<String> {
        let input_str = crate::formatting::truncate(&input.to_string(), 500);
        let prompt = format!(
            "Summarize this Claude Code tool action in under 10 words. \
             Tool: {tool}. Input: {input_str}"
        );

        if self.endpoint.contains("/api/chat") || self.endpoint.contains("/api/generate") {
            self.call_ollama(&prompt).await
        } else if self.endpoint.contains(":11434") {
            // Ollama default port — append /api/chat
            self.call_ollama(&prompt).await
        } else if self.endpoint.contains("anthropic") {
            self.call_anthropic(&prompt).await
        } else {
            self.call_generic_chat(&prompt).await
        }
    }

    async fn call_ollama(&self, prompt: &str) -> anyhow::Result<String> {
        // Normalize endpoint: ensure it ends with /api/chat
        let url = if self.endpoint.ends_with("/api/chat") || self.endpoint.ends_with("/api/generate") {
            self.endpoint.clone()
        } else {
            let base = self.endpoint.trim_end_matches('/');
            format!("{base}/api/chat")
        };

        let body = serde_json::json!({
            "model": "qwen3-coder:latest",
            "messages": [{"role": "user", "content": prompt}],
            "stream": false,
            "options": {"num_predict": 30}
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        // Ollama /api/chat: {"message": {"content": "..."}}
        // Ollama /api/generate: {"response": "..."}
        let text = resp
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .or_else(|| resp.get("response").and_then(|r| r.as_str()))
            .unwrap_or("")
            .trim()
            .to_string();
        Ok(text)
    }

    async fn call_anthropic(&self, prompt: &str) -> anyhow::Result<String> {
        let body = serde_json::json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 30,
            "messages": [{"role": "user", "content": prompt}]
        });

        let mut req = self.client.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            req = req
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01");
        }

        let resp = req.send().await?.json::<serde_json::Value>().await?;
        let text = resp
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        Ok(text)
    }

    async fn call_generic_chat(&self, prompt: &str) -> anyhow::Result<String> {
        let body = serde_json::json!({
            "prompt": prompt,
            "timeout": 4000
        });

        let resp = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let text = resp
            .get("response")
            .or_else(|| resp.get("text"))
            .or_else(|| resp.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        Ok(text)
    }
}

/// Returns true if the summary is too generic and should be enhanced by LLM.
fn is_generic_summary(summary: &str) -> bool {
    summary.starts_with("Using ") || summary.starts_with("Running `")
}

/// Build a deterministic cache key from tool name and input.
fn build_cache_key(tool: &str, input: &serde_json::Value) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    tool.hash(&mut hasher);
    input.to_string().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
