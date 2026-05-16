use std::env;

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
    pub tracked_activity_types: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, env::VarError> {
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
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
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

        let cfg = Config::from_env().unwrap();

        assert_eq!(cfg.telegram_bot_token, "test-token");
        assert_eq!(cfg.telegram_chat_id, "-123");
        assert_eq!(cfg.strava_client_id, "client-id");
        assert_eq!(cfg.strava_client_secret, "client-secret");
        assert_eq!(cfg.poll_interval_seconds, 120);
        assert_eq!(cfg.cold_start_lookback_days, 30); // default
        assert_eq!(cfg.database_path, "/tmp/test.db");
        assert_eq!(cfg.bot_admin_usernames, vec!["alice", "bob"]);
        assert_eq!(cfg.tracked_activity_types, vec!["Run", "Hike"]);
    }

    #[test]
    fn test_defaults() {
        let _guard = CONFIG_LOCK.lock().unwrap();
        env::remove_var("POLL_INTERVAL_SECONDS");
        env::remove_var("BOT_ADMIN_USERNAMES");
        env::remove_var("DATABASE_PATH");
        env::remove_var("COLD_START_LOOKBACK_DAYS");
        env::remove_var("TRACKED_ACTIVITY_TYPES");
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
            vec!["Run", "TrailRun", "VirtualRun", "Hike", "Walk"]
        );
    }
}
