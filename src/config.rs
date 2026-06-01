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

/// A single Strava OAuth app's credentials.
#[derive(Clone, Debug)]
pub struct StravaApp {
    pub id: String,
    pub secret: String,
}

/// Temporary workaround: athletes are pinned to a Strava app slot because
/// each app has a 1-athlete cap by default. `slot_2` becomes `None` once
/// app 1's quota is raised and the workaround is reverted.
#[derive(Clone, Debug)]
pub struct StravaApps {
    pub slot_1: StravaApp,
    pub slot_2: Option<StravaApp>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub strava_apps: StravaApps,
    pub poll_interval_seconds: u64,
    pub cold_start_lookback_days: i64,
    pub database_path: String,
    pub bot_admin_usernames: Vec<String>,
    pub tracked_activity_types: Vec<ActivityType>,
    pub notification_mode: NotificationMode,
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

        let strava_apps = load_strava_apps()?;

        Ok(Self {
            telegram_bot_token: env::var("TELEGRAM_BOT_TOKEN")?,
            telegram_chat_id: env::var("TELEGRAM_CHAT_ID")?,
            strava_apps,
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
        })
    }
}

fn parse_env_default<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn load_strava_apps() -> anyhow::Result<StravaApps> {
    let slot_1 = StravaApp {
        id: env::var("STRAVA_CLIENT_ID")?,
        secret: env::var("STRAVA_CLIENT_SECRET")?,
    };

    // Slot 2 is optional; both vars must be set together or both absent.
    let id_2 = env::var("STRAVA_CLIENT_ID_2").ok();
    let secret_2 = env::var("STRAVA_CLIENT_SECRET_2").ok();
    let slot_2 = match (id_2, secret_2) {
        (Some(id), Some(secret)) => Some(StravaApp { id, secret }),
        (None, None) => None,
        (Some(_), None) => {
            anyhow::bail!("STRAVA_CLIENT_ID_2 is set but STRAVA_CLIENT_SECRET_2 is not")
        }
        (None, Some(_)) => {
            anyhow::bail!("STRAVA_CLIENT_SECRET_2 is set but STRAVA_CLIENT_ID_2 is not")
        }
    };

    Ok(StravaApps { slot_1, slot_2 })
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
        env::remove_var("STRAVA_CLIENT_ID_2");
        env::remove_var("STRAVA_CLIENT_SECRET_2");
        env::set_var("POLL_INTERVAL_SECONDS", "120");
        env::set_var("DATABASE_PATH", "/tmp/test.db");
        env::set_var("BOT_ADMIN_USERNAMES", "alice,bob");
        env::set_var("TRACKED_ACTIVITY_TYPES", "Run,Hike");
        env::set_var("NOTIFICATION_MODE", "text_only");

        let cfg = Config::from_env().unwrap();

        assert_eq!(cfg.telegram_bot_token, "test-token");
        assert_eq!(cfg.telegram_chat_id, "-123");
        assert_eq!(cfg.strava_apps.slot_1.id, "client-id");
        assert_eq!(cfg.strava_apps.slot_1.secret, "client-secret");
        assert!(cfg.strava_apps.slot_2.is_none());
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
    fn test_slot_2_loaded_when_both_vars_set() {
        let _guard = CONFIG_LOCK.lock().unwrap();
        env::set_var("TELEGRAM_BOT_TOKEN", "t");
        env::set_var("TELEGRAM_CHAT_ID", "c");
        env::set_var("STRAVA_CLIENT_ID", "id1");
        env::set_var("STRAVA_CLIENT_SECRET", "sec1");
        env::set_var("STRAVA_CLIENT_ID_2", "id2");
        env::set_var("STRAVA_CLIENT_SECRET_2", "sec2");

        let cfg = Config::from_env().unwrap();
        let app2 = cfg.strava_apps.slot_2.expect("slot 2 should be Some");
        assert_eq!(app2.id, "id2");
        assert_eq!(app2.secret, "sec2");

        env::remove_var("STRAVA_CLIENT_ID_2");
        env::remove_var("STRAVA_CLIENT_SECRET_2");
    }

    #[test]
    fn test_asymmetric_slot_2_config_errors() {
        let _guard = CONFIG_LOCK.lock().unwrap();
        env::set_var("TELEGRAM_BOT_TOKEN", "t");
        env::set_var("TELEGRAM_CHAT_ID", "c");
        env::set_var("STRAVA_CLIENT_ID", "id1");
        env::set_var("STRAVA_CLIENT_SECRET", "sec1");

        // Only ID set
        env::set_var("STRAVA_CLIENT_ID_2", "id2");
        env::remove_var("STRAVA_CLIENT_SECRET_2");
        assert!(Config::from_env().is_err());

        // Only secret set
        env::remove_var("STRAVA_CLIENT_ID_2");
        env::set_var("STRAVA_CLIENT_SECRET_2", "sec2");
        assert!(Config::from_env().is_err());

        env::remove_var("STRAVA_CLIENT_SECRET_2");
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
}
