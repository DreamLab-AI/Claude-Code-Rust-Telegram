//! Load CTM configuration from agentbox.toml with env var overlay.
//!
//! Priority (highest wins): env vars > agentbox.toml > legacy config.json > defaults.
//! The bot_token is NEVER read from TOML — only from TELEGRAM_BOT_TOKEN env var
//! or the legacy config.json (for backward compatibility during migration).

use crate::config::{self, Config};
use crate::error::{AppError, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Paths searched for agentbox.toml, in priority order.
const TOML_SEARCH_PATHS: &[&str] = &[
    "/home/devuser/workspace/project/agentbox/agentbox.toml",
    "/opt/agentbox/agentbox.toml",
    "./agentbox.toml",
];

/// Represents a single allowed user entry from agentbox.toml.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AllowedUser {
    #[serde(default)]
    pub pubkey_hex: String,
    #[serde(default)]
    pub telegram_id: i64,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub label: String,
}

/// Summarizer config from agentbox.toml.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SummarizerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
}

/// The [sovereign_mesh.telegram] section of agentbox.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramSection {
    #[serde(default)]
    pub chat_id: i64,
    #[serde(default = "default_true")]
    pub use_threads: bool,
    #[serde(default = "default_chunk_size")]
    pub chunk_size: usize,
    #[serde(default = "default_rate_limit")]
    pub rate_limit: u32,
    #[serde(default = "default_session_timeout")]
    pub session_timeout_minutes: u32,
    #[serde(default = "default_stale_hours")]
    pub stale_session_hours: u32,
    #[serde(default = "default_true")]
    pub auto_delete_topics: bool,
    #[serde(default = "default_topic_delay")]
    pub topic_delete_delay_minutes: u32,
    #[serde(default = "default_inactivity")]
    pub inactivity_threshold_minutes: u32,
    #[serde(default)]
    pub socket_path: String,
    #[serde(default = "default_true")]
    pub verbose: bool,
    #[serde(default)]
    pub approvals: bool,
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default = "default_max_workers")]
    pub max_workers: u32,
    #[serde(default = "default_notification_mode")]
    pub notification_mode: String,
    #[serde(default)]
    pub allowed_users: Vec<AllowedUser>,
    #[serde(default)]
    pub summarizer: SummarizerConfig,
}

/// The [sovereign_mesh.operator] section.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OperatorSection {
    #[serde(default)]
    pub pubkey_hex: String,
    #[serde(default)]
    pub npub: String,
    #[serde(default)]
    pub display_name: String,
}

/// The [sovereign_mesh] section.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SovereignMeshSection {
    #[serde(default)]
    pub telegram_mirror: bool,
    #[serde(default)]
    pub telegram: TelegramSection,
    #[serde(default)]
    pub operator: OperatorSection,
}

/// Root-level agentbox.toml structure (only the parts CTM needs).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentboxToml {
    #[serde(default)]
    pub sovereign_mesh: SovereignMeshSection,
}

impl Default for TelegramSection {
    fn default() -> Self {
        Self {
            chat_id: 0,
            use_threads: true,
            chunk_size: 4000,
            rate_limit: 20,
            session_timeout_minutes: 30,
            stale_session_hours: 72,
            auto_delete_topics: true,
            topic_delete_delay_minutes: 15,
            inactivity_threshold_minutes: 720,
            socket_path: String::new(),
            verbose: true,
            approvals: false,
            default_model: "sonnet".to_string(),
            max_workers: 5,
            notification_mode: "live".to_string(),
            allowed_users: Vec::new(),
            summarizer: SummarizerConfig::default(),
        }
    }
}

fn default_true() -> bool { true }
fn default_chunk_size() -> usize { 4000 }
fn default_rate_limit() -> u32 { 20 }
fn default_session_timeout() -> u32 { 30 }
fn default_stale_hours() -> u32 { 72 }
fn default_topic_delay() -> u32 { 15 }
fn default_inactivity() -> u32 { 720 }
fn default_model() -> String { "sonnet".to_string() }
fn default_max_workers() -> u32 { 5 }
fn default_notification_mode() -> String { "live".to_string() }

/// Find and parse agentbox.toml from known search paths.
pub fn find_agentbox_toml() -> Option<(AgentboxToml, PathBuf)> {
    // Allow override via env var
    let paths: Vec<PathBuf> = if let Ok(p) = std::env::var("AGENTBOX_TOML_PATH") {
        vec![PathBuf::from(p)]
    } else {
        TOML_SEARCH_PATHS.iter().map(PathBuf::from).collect()
    };

    for path in &paths {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => match toml::from_str::<AgentboxToml>(&content) {
                    Ok(parsed) => {
                        tracing::info!(path = %path.display(), "Loaded agentbox.toml");
                        return Some((parsed, path.clone()));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "Failed to parse agentbox.toml, skipping"
                        );
                    }
                },
                Err(e) => {
                    tracing::debug!(path = %path.display(), error = %e, "Cannot read agentbox.toml");
                }
            }
        }
    }
    None
}

/// Extended config that includes agentbox-specific fields beyond the base Config.
#[derive(Debug, Clone)]
pub struct ExtendedConfig {
    pub base: Config,
    pub operator_pubkey: String,
    pub allowed_users: Vec<AllowedUser>,
    pub default_model: String,
    pub max_workers: u32,
    pub notification_mode: String,
    pub agentbox_toml_path: Option<PathBuf>,
}

/// Load config with agentbox.toml as primary source.
///
/// Priority: env vars > agentbox.toml [sovereign_mesh.telegram] > legacy config.json > defaults.
pub fn load_extended_config(require_auth: bool) -> Result<ExtendedConfig> {
    let agentbox = find_agentbox_toml();

    let (toml_cfg, toml_path) = match &agentbox {
        Some((cfg, path)) => (Some(cfg.clone()), Some(path.clone())),
        None => {
            tracing::info!("No agentbox.toml found, falling back to legacy config");
            (None, None)
        }
    };

    // Load legacy config as the fallback base
    let mut base = config::load_config(false)?;

    // Overlay agentbox.toml values onto the base config
    if let Some(ref toml) = toml_cfg {
        let tg = &toml.sovereign_mesh.telegram;

        if tg.chat_id != 0 {
            base.chat_id = tg.chat_id;
        }
        base.use_threads = tg.use_threads;
        base.chunk_size = tg.chunk_size;
        base.rate_limit = tg.rate_limit;
        base.session_timeout = tg.session_timeout_minutes;
        base.stale_session_timeout_hours = tg.stale_session_hours;
        base.auto_delete_topics = tg.auto_delete_topics;
        base.topic_delete_delay_minutes = tg.topic_delete_delay_minutes;
        base.inactivity_delete_threshold_minutes = tg.inactivity_threshold_minutes;
        base.verbose = tg.verbose;
        base.approvals = tg.approvals;

        if !tg.socket_path.is_empty() {
            if config::validate_socket_path(&tg.socket_path) {
                base.socket_path = PathBuf::from(&tg.socket_path);
            } else {
                tracing::warn!(path = %tg.socket_path, "Invalid socket_path in agentbox.toml, using default");
            }
        }

        // LLM summarizer
        if tg.summarizer.enabled && !tg.summarizer.url.is_empty() {
            base.llm_summarize_url = Some(tg.summarizer.url.clone());
        }

        // Enable mirroring if sovereign_mesh.telegram_mirror is true
        if toml.sovereign_mesh.telegram_mirror {
            base.enabled = true;
        }
    }

    // Env vars always win (re-apply over TOML values)
    apply_env_overrides(&mut base);

    if require_auth {
        if base.bot_token.is_empty() {
            return Err(AppError::Config(
                "TELEGRAM_BOT_TOKEN is required. Set it as an environment variable.".into(),
            ));
        }
        if base.chat_id == 0 {
            return Err(AppError::Config(
                "Telegram chat_id is required. Set it in agentbox.toml [sovereign_mesh.telegram] or via TELEGRAM_CHAT_ID env var.".into(),
            ));
        }
    }

    let operator_pubkey = toml_cfg
        .as_ref()
        .map(|t| t.sovereign_mesh.operator.pubkey_hex.clone())
        .unwrap_or_default();

    let allowed_users = toml_cfg
        .as_ref()
        .map(|t| t.sovereign_mesh.telegram.allowed_users.clone())
        .unwrap_or_default();

    let default_model = toml_cfg
        .as_ref()
        .map(|t| t.sovereign_mesh.telegram.default_model.clone())
        .unwrap_or_else(default_model);

    let max_workers = toml_cfg
        .as_ref()
        .map(|t| t.sovereign_mesh.telegram.max_workers)
        .unwrap_or(default_max_workers());

    let notification_mode = toml_cfg
        .as_ref()
        .map(|t| t.sovereign_mesh.telegram.notification_mode.clone())
        .unwrap_or_else(default_notification_mode);

    Ok(ExtendedConfig {
        base,
        operator_pubkey,
        allowed_users,
        default_model,
        max_workers,
        notification_mode,
        agentbox_toml_path: toml_path,
    })
}

/// Re-apply env var overrides after TOML loading (env always wins).
fn apply_env_overrides(config: &mut Config) {
    if let Ok(v) = std::env::var("TELEGRAM_BOT_TOKEN") {
        if !v.is_empty() {
            config.bot_token = v;
        }
    }
    if let Ok(v) = std::env::var("TELEGRAM_CHAT_ID") {
        if let Ok(id) = v.parse::<i64>() {
            if id != 0 {
                config.chat_id = id;
            }
        }
    }
    if let Ok(v) = std::env::var("TELEGRAM_MIRROR") {
        config.enabled = matches!(v.as_str(), "true" | "1");
    }
    if let Ok(v) = std::env::var("TELEGRAM_MIRROR_VERBOSE") {
        config.verbose = matches!(v.as_str(), "true" | "1");
    }
    if let Ok(v) = std::env::var("TELEGRAM_USE_THREADS") {
        config.use_threads = matches!(v.as_str(), "true" | "1");
    }
    if let Ok(v) = std::env::var("TELEGRAM_CHUNK_SIZE") {
        if let Ok(n) = v.parse::<usize>() {
            config.chunk_size = n;
        }
    }
    if let Ok(v) = std::env::var("TELEGRAM_RATE_LIMIT") {
        if let Ok(n) = v.parse::<u32>() {
            config.rate_limit = n;
        }
    }
    if let Ok(v) = std::env::var("CTM_LLM_SUMMARIZE_URL") {
        if !v.is_empty() {
            config.llm_summarize_url = Some(v);
        }
    }
    if let Ok(v) = std::env::var("CTM_LLM_API_KEY") {
        if !v.is_empty() {
            config.llm_api_key = Some(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toml_section_defaults() {
        let section = TelegramSection::default();
        assert_eq!(section.chat_id, 0);
        assert!(section.use_threads);
        assert_eq!(section.chunk_size, 4000);
        assert_eq!(section.rate_limit, 20);
        assert_eq!(section.session_timeout_minutes, 30);
        assert_eq!(section.stale_session_hours, 72);
        assert!(section.auto_delete_topics);
        assert_eq!(section.topic_delete_delay_minutes, 15);
        assert_eq!(section.inactivity_threshold_minutes, 720);
        assert!(!section.approvals);
        assert_eq!(section.default_model, "sonnet");
        assert_eq!(section.max_workers, 5);
        assert_eq!(section.notification_mode, "live");
    }

    #[test]
    fn test_parse_minimal_toml() {
        let toml_str = r#"
[sovereign_mesh]
telegram_mirror = true

[sovereign_mesh.operator]
pubkey_hex = "deadbeef"

[sovereign_mesh.telegram]
chat_id = -1001234567890
"#;
        let parsed: AgentboxToml = toml::from_str(toml_str).unwrap();
        assert!(parsed.sovereign_mesh.telegram_mirror);
        assert_eq!(parsed.sovereign_mesh.operator.pubkey_hex, "deadbeef");
        assert_eq!(parsed.sovereign_mesh.telegram.chat_id, -1001234567890);
        assert!(parsed.sovereign_mesh.telegram.use_threads); // default
    }

    #[test]
    fn test_parse_allowed_users() {
        let toml_str = r#"
[sovereign_mesh.telegram]
chat_id = -100123

[[sovereign_mesh.telegram.allowed_users]]
pubkey_hex = "aabbccdd"
telegram_id = 12345
role = "admin"
label = "John"

[[sovereign_mesh.telegram.allowed_users]]
pubkey_hex = "eeff0011"
telegram_id = 67890
role = "user"
label = "Jane"
"#;
        let parsed: AgentboxToml = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.sovereign_mesh.telegram.allowed_users.len(), 2);
        assert_eq!(parsed.sovereign_mesh.telegram.allowed_users[0].role, "admin");
        assert_eq!(parsed.sovereign_mesh.telegram.allowed_users[1].telegram_id, 67890);
    }

    #[test]
    fn test_parse_summarizer() {
        let toml_str = r#"
[sovereign_mesh.telegram.summarizer]
enabled = true
url = "https://api.example.com/v1/chat"
"#;
        let parsed: AgentboxToml = toml::from_str(toml_str).unwrap();
        assert!(parsed.sovereign_mesh.telegram.summarizer.enabled);
        assert_eq!(parsed.sovereign_mesh.telegram.summarizer.url, "https://api.example.com/v1/chat");
    }
}
