//! Direct Claude CLI subprocess management.
//!
//! Replaces the tmux send-keys injection pattern with direct subprocess spawning
//! using `claude -p --output-format stream-json --resume <session_id>`.
//! Ported from jedarden's Go session manager pattern.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex, RwLock};

/// Maximum NDJSON line size from Claude CLI (1 MiB).
const MAX_LINE_BYTES: usize = 1_048_576;

/// Result of a Claude invocation.
#[derive(Debug, Clone)]
pub struct InvocationResult {
    pub session_id: String,
    pub text: String,
    pub is_error: bool,
    pub cost_usd: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub model: String,
}

/// A streaming text delta from the Claude subprocess.
#[derive(Debug, Clone)]
pub struct StreamDelta {
    pub text: String,
}

/// Events emitted during a Claude invocation.
#[derive(Debug, Clone)]
pub enum SubprocessEvent {
    Delta(StreamDelta),
    Complete(InvocationResult),
    Error(String),
}

/// Stream-JSON line types from `claude -p --output-format stream-json`.
#[derive(Debug, Deserialize)]
struct StreamLine {
    #[serde(rename = "type")]
    line_type: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    event: Option<serde_json::Value>,
    #[serde(default)]
    usage: Option<UsageInfo>,
}

#[derive(Debug, Deserialize, Default)]
struct UsageInfo {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
}

/// Configuration for a Claude subprocess invocation.
#[derive(Debug, Clone)]
pub struct InvocationConfig {
    pub cwd: String,
    pub model: String,
    pub session_id: Option<String>,
    pub allowed_tools: Option<String>,
    pub disallowed_tools: Option<String>,
    pub permission_mode: String,
    pub timeout_secs: u64,
}

impl Default for InvocationConfig {
    fn default() -> Self {
        Self {
            cwd: ".".to_string(),
            model: "sonnet".to_string(),
            session_id: None,
            allowed_tools: None,
            disallowed_tools: None,
            permission_mode: "default".to_string(),
            timeout_secs: 300,
        }
    }
}

/// Manages Claude CLI subprocess invocations per topic.
pub struct SubprocessManager {
    active: Arc<RwLock<HashMap<(i64, i64), tokio::task::JoinHandle<()>>>>,
    max_concurrent: usize,
}

impl SubprocessManager {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            active: Arc::new(RwLock::new(HashMap::new())),
            max_concurrent,
        }
    }

    /// Count currently active invocations.
    pub async fn active_count(&self) -> usize {
        let map = self.active.read().await;
        map.values().filter(|h| !h.is_finished()).count()
    }

    /// Check if a specific topic has an active invocation.
    pub async fn is_topic_busy(&self, chat_id: i64, thread_id: i64) -> bool {
        let map = self.active.read().await;
        map.get(&(chat_id, thread_id))
            .is_some_and(|h| !h.is_finished())
    }

    /// Invoke Claude CLI with the given prompt, streaming events to the channel.
    /// Returns immediately; the invocation runs in a background task.
    pub async fn invoke(
        &self,
        chat_id: i64,
        thread_id: i64,
        prompt: String,
        config: InvocationConfig,
        tx: mpsc::Sender<SubprocessEvent>,
    ) -> Result<(), String> {
        if self.is_topic_busy(chat_id, thread_id).await {
            return Err("Topic already has an active invocation".to_string());
        }

        if self.active_count().await >= self.max_concurrent {
            return Err(format!(
                "Max concurrent invocations ({}) reached",
                self.max_concurrent
            ));
        }

        let active = self.active.clone();
        let key = (chat_id, thread_id);

        let handle = tokio::spawn(async move {
            let result = run_claude_subprocess(prompt, &config, &tx).await;
            match result {
                Ok(()) => {}
                Err(e) => {
                    let _ = tx.send(SubprocessEvent::Error(e)).await;
                }
            }
        });

        let mut map = active.write().await;
        // Clean finished handles while we're here
        map.retain(|_, h| !h.is_finished());
        map.insert(key, handle);

        Ok(())
    }

    /// Cancel an active invocation for a topic.
    pub async fn cancel(&self, chat_id: i64, thread_id: i64) -> bool {
        let mut map = self.active.write().await;
        if let Some(handle) = map.remove(&(chat_id, thread_id)) {
            handle.abort();
            true
        } else {
            false
        }
    }
}

/// Run a single Claude subprocess invocation to completion.
async fn run_claude_subprocess(
    prompt: String,
    config: &InvocationConfig,
    tx: &mpsc::Sender<SubprocessEvent>,
) -> Result<(), String> {
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--model".to_string(),
        config.model.clone(),
    ];

    if let Some(ref sid) = config.session_id {
        args.push("--resume".to_string());
        args.push(sid.clone());
    }

    if config.permission_mode == "dangerously-skip" {
        args.push("--dangerously-skip-permissions".to_string());
    }

    if let Some(ref allowed) = config.allowed_tools {
        if !allowed.is_empty() {
            args.push("--allowed-tools".to_string());
            args.push(allowed.clone());
        }
    }

    if let Some(ref disallowed) = config.disallowed_tools {
        if !disallowed.is_empty() {
            args.push("--disallowed-tools".to_string());
            args.push(disallowed.clone());
        }
    }

    let mut cmd = Command::new("claude");
    cmd.args(&args)
        .current_dir(&config.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("spawn claude: {e}"))?;

    // Write prompt to stdin and close
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(prompt.as_bytes()).await;
        drop(stdin);
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "no stdout pipe".to_string())?;

    let mut reader = BufReader::with_capacity(MAX_LINE_BYTES, stdout);
    let mut line_buf = String::new();
    let mut text_buf = String::new();
    let mut final_result: Option<InvocationResult> = None;

    loop {
        line_buf.clear();
        let bytes_read = reader
            .read_line(&mut line_buf)
            .await
            .map_err(|e| format!("read stdout: {e}"))?;

        if bytes_read == 0 {
            break; // EOF
        }

        let Ok(line) = serde_json::from_str::<StreamLine>(line_buf.trim()) else {
            continue;
        };

        match line.line_type.as_str() {
            "system" => {
                // Capture session_id from system message
                if let Some(ref sid) = line.session_id {
                    if final_result.is_none() {
                        final_result = Some(InvocationResult {
                            session_id: sid.clone(),
                            text: String::new(),
                            is_error: false,
                            cost_usd: 0.0,
                            input_tokens: 0,
                            output_tokens: 0,
                            cache_read_tokens: 0,
                            cache_creation_tokens: 0,
                            model: config.model.clone(),
                        });
                    }
                }
            }
            "stream_event" => {
                if let Some(event) = &line.event {
                    if let Some(delta_text) = extract_text_delta(event) {
                        text_buf.push_str(&delta_text);
                        let _ = tx
                            .send(SubprocessEvent::Delta(StreamDelta {
                                text: delta_text,
                            }))
                            .await;
                    }
                }
            }
            "result" => {
                let result_text = line.result.unwrap_or_default();
                let usage = line.usage.unwrap_or_default();
                let session_id = line
                    .session_id
                    .or_else(|| final_result.as_ref().map(|r| r.session_id.clone()))
                    .unwrap_or_default();

                final_result = Some(InvocationResult {
                    session_id,
                    text: if result_text.is_empty() {
                        text_buf.clone()
                    } else {
                        result_text
                    },
                    is_error: line.is_error,
                    cost_usd: line.total_cost_usd.unwrap_or(0.0),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_tokens: usage.cache_read_input_tokens,
                    cache_creation_tokens: usage.cache_creation_input_tokens,
                    model: config.model.clone(),
                });
            }
            _ => {}
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("wait claude: {e}"))?;

    if let Some(result) = final_result {
        let _ = tx.send(SubprocessEvent::Complete(result)).await;
    } else if !status.success() {
        let _ = tx
            .send(SubprocessEvent::Error(format!(
                "claude exited with status {}",
                status
            )))
            .await;
    } else {
        let _ = tx
            .send(SubprocessEvent::Complete(InvocationResult {
                session_id: String::new(),
                text: text_buf,
                is_error: false,
                cost_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                model: config.model.clone(),
            }))
            .await;
    }

    Ok(())
}

/// Extract text delta from a stream_event's nested event structure.
fn extract_text_delta(event: &serde_json::Value) -> Option<String> {
    let delta = event.get("delta")?;
    if delta.get("type")?.as_str()? == "text_delta" {
        delta.get("text")?.as_str().map(|s| s.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invocation_config_defaults() {
        let cfg = InvocationConfig::default();
        assert_eq!(cfg.model, "sonnet");
        assert_eq!(cfg.timeout_secs, 300);
        assert!(cfg.session_id.is_none());
    }

    #[test]
    fn test_extract_text_delta() {
        let event = serde_json::json!({
            "type": "content_block_delta",
            "delta": {
                "type": "text_delta",
                "text": "Hello world"
            }
        });
        assert_eq!(extract_text_delta(&event), Some("Hello world".to_string()));

        let non_text = serde_json::json!({
            "type": "content_block_delta",
            "delta": {
                "type": "tool_use_delta",
                "text": "not this"
            }
        });
        assert_eq!(extract_text_delta(&non_text), None);

        let no_delta = serde_json::json!({"type": "content_block_start"});
        assert_eq!(extract_text_delta(&no_delta), None);
    }

    #[tokio::test]
    async fn test_subprocess_manager_concurrency() {
        let mgr = SubprocessManager::new(3);
        assert_eq!(mgr.active_count().await, 0);
        assert!(!mgr.is_topic_busy(-100, 1).await);
    }

    #[test]
    fn test_parse_stream_line() {
        let line = r#"{"type":"system","session_id":"abc-123"}"#;
        let parsed: StreamLine = serde_json::from_str(line).unwrap();
        assert_eq!(parsed.line_type, "system");
        assert_eq!(parsed.session_id.as_deref(), Some("abc-123"));

        let result = r#"{"type":"result","result":"Done","is_error":false,"total_cost_usd":0.05,"usage":{"input_tokens":100,"output_tokens":50}}"#;
        let parsed: StreamLine = serde_json::from_str(result).unwrap();
        assert_eq!(parsed.line_type, "result");
        assert!(!parsed.is_error);
        assert_eq!(parsed.total_cost_usd, Some(0.05));
        assert_eq!(parsed.usage.as_ref().unwrap().input_tokens, 100);
    }
}
