use std::env;
use std::str::FromStr;

use strum::VariantNames;

use crate::types::ActivityType;

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::EnumString, strum::VariantNames)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum NotificationMode {
    CardOnly,
    TextOnly,
    CardAndText,
}

/// Settings for the AI activity-comment feature.
///
/// `Debug` is hand-written rather than derived: `Config` derives `Debug` and
/// is logged at startup, so a derived impl would print the API key.
#[derive(Clone)]
pub struct AiConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub history_limit: usize,
    pub timeout_seconds: u64,
    pub max_chars: usize,
    pub system_prompt: Option<String>,
}

impl std::fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiConfig")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("history_limit", &self.history_limit)
            .field("timeout_seconds", &self.timeout_seconds)
            .field("max_chars", &self.max_chars)
            .field(
                "system_prompt",
                &self.system_prompt.as_ref().map(|_| "<custom>"),
            )
            .finish()
    }
}

impl AiConfig {
    /// Returns `None` when `AI_API_KEY` is unset or blank, which disables the
    /// whole feature. Everything else has a default.
    fn from_env() -> Option<Self> {
        let api_key = env::var("AI_API_KEY")
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())?;

        Some(Self {
            api_key,
            base_url: env::var("AI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            model: env::var("AI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
            history_limit: parse_env_default("AI_HISTORY_LIMIT", 30),
            timeout_seconds: parse_env_default("AI_TIMEOUT_SECONDS", 20),
            max_chars: parse_env_default("AI_MAX_CHARS", 280),
            system_prompt: env::var("AI_SYSTEM_PROMPT")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub strava_client_id: String,
    pub strava_client_secret: String,
    pub poll_interval_seconds: u64,
    pub cold_start_lookback_days: i64,
    pub database_path: String,
    pub bot_admin_usernames: Vec<String>,
    pub tracked_activity_types: Vec<ActivityType>,
    pub notification_mode: NotificationMode,
    pub ai: Option<AiConfig>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let notification_mode = match env::var("NOTIFICATION_MODE") {
            Ok(v) => {
                let normalized = v.trim().replace('-', "_");
                NotificationMode::from_str(&normalized).map_err(|_| {
                    anyhow::anyhow!(
                        "invalid NOTIFICATION_MODE '{}' (expected one of: {})",
                        v,
                        NotificationMode::VARIANTS.join(", ")
                    )
                })?
            }
            Err(env::VarError::NotPresent) => NotificationMode::CardAndText,
            Err(e) => return Err(e.into()),
        };

        Ok(Self {
            telegram_bot_token: env::var("TELEGRAM_BOT_TOKEN")?,
            telegram_chat_id: env::var("TELEGRAM_CHAT_ID")?,
            strava_client_id: env::var("STRAVA_CLIENT_ID")?,
            strava_client_secret: env::var("STRAVA_CLIENT_SECRET")?,
            poll_interval_seconds: parse_env_default("POLL_INTERVAL_SECONDS", 300),
            cold_start_lookback_days: parse_env_default("COLD_START_LOOKBACK_DAYS", 30),
            database_path: env::var("DATABASE_PATH").unwrap_or_else(|_| "./data/bot.db".into()),
            bot_admin_usernames: env::var("BOT_ADMIN_USERNAMES")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            tracked_activity_types: env::var("TRACKED_ACTIVITY_TYPES")
                .unwrap_or_else(|_| "Run,TrailRun,VirtualRun,Hike,Walk".into())
                .split(',')
                .map(|s| ActivityType::from_str(s.trim()).unwrap_or(ActivityType::Other))
                .filter(|t| *t != ActivityType::Other)
                .collect(),
            notification_mode,
            ai: AiConfig::from_env(),
        })
    }
}

fn parse_env_default<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::Mutex;

    static CONFIG_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_parse_config_from_env() {
        let _guard = CONFIG_LOCK.lock().unwrap();
        env::set_var("TELEGRAM_BOT_TOKEN", "test-token");
        env::set_var("TELEGRAM_CHAT_ID", "-123");
        env::set_var("STRAVA_CLIENT_ID", "client-id");
        env::set_var("STRAVA_CLIENT_SECRET", "client-secret");
        env::set_var("POLL_INTERVAL_SECONDS", "120");
        env::set_var("DATABASE_PATH", "/tmp/test.db");
        env::set_var("BOT_ADMIN_USERNAMES", "alice,bob");
        env::set_var("TRACKED_ACTIVITY_TYPES", "Run,Hike");
        env::set_var("NOTIFICATION_MODE", "text_only");

        let cfg = Config::from_env().unwrap();

        assert_eq!(cfg.telegram_bot_token, "test-token");
        assert_eq!(cfg.telegram_chat_id, "-123");
        assert_eq!(cfg.strava_client_id, "client-id");
        assert_eq!(cfg.strava_client_secret, "client-secret");
        assert_eq!(cfg.poll_interval_seconds, 120);
        assert_eq!(cfg.cold_start_lookback_days, 30); // default
        assert_eq!(cfg.database_path, "/tmp/test.db");
        assert_eq!(cfg.bot_admin_usernames, vec!["alice", "bob"]);
        assert_eq!(
            cfg.tracked_activity_types,
            vec![ActivityType::Run, ActivityType::Hike]
        );
        assert_eq!(cfg.notification_mode, NotificationMode::TextOnly);
    }

    #[test]
    fn test_defaults() {
        let _guard = CONFIG_LOCK.lock().unwrap();
        env::remove_var("POLL_INTERVAL_SECONDS");
        env::remove_var("BOT_ADMIN_USERNAMES");
        env::remove_var("DATABASE_PATH");
        env::remove_var("COLD_START_LOOKBACK_DAYS");
        env::remove_var("TRACKED_ACTIVITY_TYPES");
        env::remove_var("NOTIFICATION_MODE");
        env::remove_var("AI_API_KEY");
        env::set_var("TELEGRAM_BOT_TOKEN", "t");
        env::set_var("TELEGRAM_CHAT_ID", "c");
        env::set_var("STRAVA_CLIENT_ID", "id");
        env::set_var("STRAVA_CLIENT_SECRET", "secret");

        let cfg = Config::from_env().unwrap();

        assert_eq!(cfg.poll_interval_seconds, 300);
        assert_eq!(cfg.cold_start_lookback_days, 30);
        assert_eq!(cfg.database_path, "./data/bot.db");
        assert!(cfg.bot_admin_usernames.is_empty());
        assert_eq!(
            cfg.tracked_activity_types,
            vec![
                ActivityType::Run,
                ActivityType::TrailRun,
                ActivityType::VirtualRun,
                ActivityType::Hike,
                ActivityType::Walk,
            ]
        );
        assert_eq!(cfg.notification_mode, NotificationMode::CardAndText);
    }

    #[test]
    fn test_notification_mode_from_str() {
        // strum-derived FromStr: snake_case, case-insensitive
        assert_eq!(
            NotificationMode::from_str("card_only").unwrap(),
            NotificationMode::CardOnly
        );
        assert_eq!(
            NotificationMode::from_str("TEXT_ONLY").unwrap(),
            NotificationMode::TextOnly
        );
        assert_eq!(
            NotificationMode::from_str("Card_And_Text").unwrap(),
            NotificationMode::CardAndText
        );
        assert!(NotificationMode::from_str("bogus").is_err());
    }

    #[test]
    fn test_from_env_accepts_hyphenated_and_whitespace() {
        let _guard = CONFIG_LOCK.lock().unwrap();
        env::set_var("TELEGRAM_BOT_TOKEN", "t");
        env::set_var("TELEGRAM_CHAT_ID", "c");
        env::set_var("STRAVA_CLIENT_ID", "id");
        env::set_var("STRAVA_CLIENT_SECRET", "secret");
        env::set_var("NOTIFICATION_MODE", "  Card-And-Text  ");

        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.notification_mode, NotificationMode::CardAndText);

        env::remove_var("NOTIFICATION_MODE");
    }

    #[test]
    fn test_invalid_notification_mode_fails() {
        let _guard = CONFIG_LOCK.lock().unwrap();
        env::set_var("TELEGRAM_BOT_TOKEN", "t");
        env::set_var("TELEGRAM_CHAT_ID", "c");
        env::set_var("STRAVA_CLIENT_ID", "id");
        env::set_var("STRAVA_CLIENT_SECRET", "secret");
        env::set_var("NOTIFICATION_MODE", "bogus");

        let err = Config::from_env().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid NOTIFICATION_MODE"));
        // Sanity-check that the error lists the valid variants
        assert!(msg.contains("card_only"));
        assert!(msg.contains("text_only"));
        assert!(msg.contains("card_and_text"));

        env::remove_var("NOTIFICATION_MODE");
    }

    #[test]
    fn test_ai_disabled_when_no_api_key() {
        let _guard = CONFIG_LOCK.lock().unwrap();
        env::remove_var("AI_API_KEY");
        env::set_var("TELEGRAM_BOT_TOKEN", "t");
        env::set_var("TELEGRAM_CHAT_ID", "c");
        env::set_var("STRAVA_CLIENT_ID", "id");
        env::set_var("STRAVA_CLIENT_SECRET", "secret");

        let cfg = Config::from_env().unwrap();
        assert!(cfg.ai.is_none());
    }

    #[test]
    fn test_ai_blank_api_key_is_disabled() {
        let _guard = CONFIG_LOCK.lock().unwrap();
        env::set_var("TELEGRAM_BOT_TOKEN", "t");
        env::set_var("TELEGRAM_CHAT_ID", "c");
        env::set_var("STRAVA_CLIENT_ID", "id");
        env::set_var("STRAVA_CLIENT_SECRET", "secret");
        env::set_var("AI_API_KEY", "   ");

        let cfg = Config::from_env().unwrap();
        assert!(cfg.ai.is_none());

        env::remove_var("AI_API_KEY");
    }

    #[test]
    fn test_ai_enabled_with_defaults() {
        let _guard = CONFIG_LOCK.lock().unwrap();
        env::set_var("TELEGRAM_BOT_TOKEN", "t");
        env::set_var("TELEGRAM_CHAT_ID", "c");
        env::set_var("STRAVA_CLIENT_ID", "id");
        env::set_var("STRAVA_CLIENT_SECRET", "secret");
        env::set_var("AI_API_KEY", "sk-test");
        env::remove_var("AI_BASE_URL");
        env::remove_var("AI_MODEL");
        env::remove_var("AI_HISTORY_LIMIT");
        env::remove_var("AI_TIMEOUT_SECONDS");
        env::remove_var("AI_MAX_CHARS");
        env::remove_var("AI_SYSTEM_PROMPT");

        let ai = Config::from_env().unwrap().ai.unwrap();
        assert_eq!(ai.api_key, "sk-test");
        assert_eq!(ai.base_url, "https://api.openai.com/v1");
        assert_eq!(ai.model, "gpt-4o-mini");
        assert_eq!(ai.history_limit, 30);
        assert_eq!(ai.timeout_seconds, 20);
        assert_eq!(ai.max_chars, 280);
        assert!(ai.system_prompt.is_none());

        env::remove_var("AI_API_KEY");
    }

    #[test]
    fn test_ai_debug_redacts_api_key() {
        let ai = AiConfig {
            api_key: "sk-supersecret".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            history_limit: 30,
            timeout_seconds: 20,
            max_chars: 280,
            system_prompt: Some("custom persona".into()),
        };
        let rendered = format!("{:?}", ai);
        assert!(!rendered.contains("sk-supersecret"));
        assert!(!rendered.contains("custom persona"));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("gpt-4o-mini"));
    }
}
