use log::info;
use std::sync::Arc;
use teloxide::{
    prelude::*,
    types::{InputFile, ParseMode},
    utils::command::BotCommands,
    RequestError,
};

use crate::config::Config;
use crate::db::{self, Db};
use crate::strava::{self, StravaClients, TokenResponse};
use crate::types::Slot;

/// Why slot resolution failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotError {
    /// Both Strava app slots already have an athlete pinned to them.
    BothFull,
    /// Slot 1 is occupied and slot 2 isn't configured in the environment.
    Slot1FullApp2NotConfigured,
}

impl std::fmt::Display for SlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BothFull => f.write_str("Both Strava app slots are full."),
            Self::Slot1FullApp2NotConfigured => f.write_str(
                "Slot 1 is full. Set STRAVA_CLIENT_ID_2 and STRAVA_CLIENT_SECRET_2 to enable slot 2.",
            ),
        }
    }
}

/// Decide which Strava app slot a `/register` or `/auth` call should use.
///
/// If the athlete already exists, reuse their existing slot (re-authorization
/// case). Otherwise fill the empty slot, preferring slot 1.
pub fn resolve_slot(
    conn: &rusqlite::Connection,
    strava_id: i64,
    config: &Config,
) -> anyhow::Result<Result<Slot, SlotError>> {
    if let Some(existing) = db::get_athlete(conn, strava_id)? {
        return Ok(Ok(existing.strava_app_slot));
    }

    let athletes = db::list_athletes(conn)?;
    let slot_1_taken = athletes.iter().any(|a| a.strava_app_slot == Slot::One);
    let slot_2_taken = athletes.iter().any(|a| a.strava_app_slot == Slot::Two);

    if !slot_1_taken {
        return Ok(Ok(Slot::One));
    }
    // slot 1 is taken
    if config.strava_apps.slot_2.is_none() {
        return Ok(Err(SlotError::Slot1FullApp2NotConfigured));
    }
    if slot_2_taken {
        return Ok(Err(SlotError::BothFull));
    }
    Ok(Ok(Slot::Two))
}

/// Convert a displayable error into a `RequestError` (for use in ? propagation).
fn to_request_error(e: impl std::fmt::Display) -> RequestError {
    RequestError::Io(std::sync::Arc::new(std::io::Error::other(e.to_string())))
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    #[command(description = "Show this help message")]
    Help,
    #[command(description = "List all tracked athletes")]
    List,
    #[command(description = "Show stats for an athlete")]
    Strava(String),
    #[command(description = "Register a new athlete (admin only)")]
    Register(String),
    #[command(description = "Authorize an athlete with OAuth code (admin only)")]
    Auth(String),
    #[command(description = "Show the latest activity for an athlete with card image")]
    Latest(String),
}

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: Arc<Db>,
    pub strava_clients: Arc<StravaClients>,
    pub poll_tx: tokio::sync::mpsc::UnboundedSender<crate::poller::PollCommand>,
}

#[must_use]
pub fn sender_username(msg: &Message) -> String {
    msg.from
        .as_ref()
        .and_then(|u| u.username.as_deref())
        .unwrap_or("")
        .to_lowercase()
}

#[must_use]
pub fn is_admin(msg: &Message, config: &Config) -> bool {
    let username = sender_username(msg);
    config
        .bot_admin_usernames
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&username))
}

pub async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    match cmd {
        Command::Help => cmd_help(bot, msg).await,
        Command::List => cmd_list(bot, msg, state).await,
        Command::Strava(name) => cmd_strava(bot, msg, name, state).await,
        Command::Register(args) => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            let name = parts.first().copied().unwrap_or("").to_string();
            let strava_id = parts.get(1).copied().unwrap_or("").to_string();
            cmd_register(bot, msg, name, strava_id, state).await
        }
        Command::Auth(args) => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            let name = parts.first().copied().unwrap_or("").to_string();
            let strava_id = parts.get(1).copied().unwrap_or("").to_string();
            let code = parts.get(2).copied().unwrap_or("").to_string();
            cmd_auth(bot, msg, name, strava_id, code, state).await
        }
        Command::Latest(name) => cmd_latest(bot, msg, name, state).await,
    }
}

async fn cmd_help(bot: Bot, msg: Message) -> ResponseResult<()> {
    bot.send_message(msg.chat.id, Command::descriptions().to_string())
        .await?;
    Ok(())
}

async fn cmd_list(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    let db = Arc::clone(&state.db);
    let athletes = tokio::task::spawn_blocking(move || db.run(db::list_athletes))
        .await
        .map_err(|e| {
            log::error!("DB join error: {}", e);
            to_request_error(e)
        })?
        .unwrap_or_else(|e| {
            log::error!("DB error listing athletes: {}", e);
            vec![]
        });

    let text = if athletes.is_empty() {
        "No athletes tracked yet.".to_string()
    } else {
        athletes
            .iter()
            .map(|a| {
                format!(
                    "• {} (Strava ID: {}, app slot: {})",
                    a.name, a.strava_id, a.strava_app_slot as u8,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    bot.send_message(msg.chat.id, text).await?;
    Ok(())
}

async fn cmd_strava(
    bot: Bot,
    msg: Message,
    name: String,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    let db = Arc::clone(&state.db);
    let name_clone = name.clone();

    let result = tokio::task::spawn_blocking(move || {
        db.run(|conn| {
            let athletes = db::list_athletes(conn)?;
            let athlete = athletes
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case(&name_clone));

            let Some(athlete) = athlete else {
                return Ok(format!("Athlete '{}' not found.", name_clone));
            };

            let latest = db::get_latest_activity(conn, athlete.strava_id)?;
            let (monday, first_of_month) = db::period_boundaries();
            let week = db::get_week_km(conn, athlete.strava_id, monday)?;
            let month = db::get_month_km(conn, athlete.strava_id, first_of_month)?;
            let oldest = db::get_oldest_activity_date(conn, athlete.strava_id)?;

            let (inc_week, inc_month) = crate::formatting::incomplete_periods(oldest);

            let mut text = format!(
                "{} — Week: {:.1} km · Month: {:.1} km",
                athlete.name, week, month,
            );

            if inc_week {
                text.push_str("\n⚠️ Week stats may be incomplete");
            }
            if inc_month {
                text.push_str("\n⚠️ Month stats may be incomplete");
            }

            if let Some(act) = latest {
                text.push_str(&format!(
                    "\n\nLatest: {} — {:.1} km — {}",
                    act.title, act.distance_km, act.url,
                ));
            }

            Ok(text)
        })
    })
    .await
    .map_err(|e| {
        log::error!("DB join error: {}", e);
        to_request_error(e)
    })?
    .map_err(|e| {
        log::error!("DB error: {}", e);
        to_request_error(e)
    })?;

    bot.send_message(msg.chat.id, result).await?;
    Ok(())
}

async fn cmd_register(
    bot: Bot,
    msg: Message,
    name: String,
    strava_id: String,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    if !is_admin(&msg, &state.config) {
        bot.send_message(msg.chat.id, "This command is restricted to admin users.")
            .await?;
        return Ok(());
    }

    let Ok(strava_id_int) = strava_id.parse::<i64>() else {
        bot.send_message(msg.chat.id, "Invalid Strava ID — must be a number.")
            .await?;
        return Ok(());
    };

    let db = Arc::clone(&state.db);
    let config = state.config.clone();
    let slot_result = tokio::task::spawn_blocking(move || {
        db.run(|conn| resolve_slot(conn, strava_id_int, &config))
    })
    .await
    .map_err(|e| {
        log::error!("DB join error: {}", e);
        to_request_error(e)
    })?
    .map_err(|e| {
        log::error!("DB error during slot resolution: {}", e);
        to_request_error(e)
    })?;

    let slot = match slot_result {
        Ok(slot) => slot,
        Err(slot_err) => {
            bot.send_message(msg.chat.id, slot_err.to_string()).await?;
            return Ok(());
        }
    };

    let app = match slot {
        Slot::One => &state.config.strava_apps.slot_1,
        Slot::Two => state
            .config
            .strava_apps
            .slot_2
            .as_ref()
            .expect("resolve_slot returned Two ⇒ slot 2 is configured"),
    };

    let auth_url = format!(
        "https://www.strava.com/oauth/authorize?client_id={}&redirect_uri=http://localhost&response_type=code&approval_prompt=force&scope=read,activity:read,activity:read_all",
        app.id
    );

    bot.send_message(
        msg.chat.id,
        format!(
            "Send this link to the athlete (Strava ID: {}, Strava app slot: {}):\n\n{}\n\n\
             Once authorized, use:\n/auth {} {} <code>",
            strava_id, slot as u8, auth_url, name, strava_id,
        ),
    )
    .await?;
    info!(
        "Registration link generated for {} ({}) → slot {}",
        name, strava_id, slot as u8
    );
    Ok(())
}

async fn cmd_auth(
    bot: Bot,
    msg: Message,
    name: String,
    strava_id: String,
    code: String,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    if !is_admin(&msg, &state.config) {
        bot.send_message(msg.chat.id, "This command is restricted to admin users.")
            .await?;
        return Ok(());
    }

    let strava_id_int: i64 = if let Ok(id) = strava_id.parse() {
        id
    } else {
        bot.send_message(msg.chat.id, "Invalid Strava ID — must be a number.")
            .await?;
        return Ok(());
    };

    // Resolve which Strava app slot this athlete belongs to.
    let db = Arc::clone(&state.db);
    let config = state.config.clone();
    let slot_result = tokio::task::spawn_blocking(move || {
        db.run(|conn| resolve_slot(conn, strava_id_int, &config))
    })
    .await
    .map_err(|e| {
        log::error!("DB join error: {}", e);
        to_request_error(e)
    })?
    .map_err(|e| {
        log::error!("DB error during slot resolution: {}", e);
        to_request_error(e)
    })?;

    let slot = match slot_result {
        Ok(slot) => slot,
        Err(slot_err) => {
            bot.send_message(msg.chat.id, slot_err.to_string()).await?;
            return Ok(());
        }
    };

    let client = match strava::client_for_slot(&state.strava_clients, slot) {
        Ok(c) => Arc::clone(c),
        Err(e) => {
            bot.send_message(msg.chat.id, format!("Strava app config error: {}", e))
                .await?;
            return Ok(());
        }
    };

    let token: TokenResponse = match client.exchange_code(&code).await {
        Ok(t) => t,
        Err(e) => {
            bot.send_message(msg.chat.id, format!("Strava OAuth failed: {}", e))
                .await?;
            return Ok(());
        }
    };

    // Save to DB
    let db = Arc::clone(&state.db);
    let n = name.clone();
    let acc = token.access_token.clone();
    let refr = token.refresh_token.clone();
    let exp = token.expires_at;

    tokio::task::spawn_blocking(move || {
        db.run(|conn| db::insert_athlete(conn, strava_id_int, &n, &acc, &refr, exp, slot))
    })
    .await
    .map_err(|e| {
        log::error!("DB join error: {}", e);
        to_request_error(e)
    })?
    .map_err(|e| {
        log::error!("DB error: {}", e);
        to_request_error(e)
    })?;

    bot.send_message(
        msg.chat.id,
        format!("{} authorized successfully! Cold-starting...", name),
    )
    .await?;

    info!("Athlete {} ({}) authorized", name, strava_id_int);

    // Notify the poller to cold-start this athlete immediately
    let _ = state
        .poll_tx
        .send(crate::poller::PollCommand::ColdStart(strava_id_int));

    Ok(())
}

async fn cmd_latest(
    bot: Bot,
    msg: Message,
    name: String,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    let db = Arc::clone(&state.db);
    let db2 = Arc::clone(&state.db);
    let name_clone = name.clone();

    let result = tokio::task::spawn_blocking(move || {
        db.run(|conn| {
            let athletes = db::list_athletes(conn)?;
            let athlete = athletes
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case(&name_clone));

            let Some(athlete) = athlete else {
                return Ok(None);
            };

            let activity = db::get_latest_activity(conn, athlete.strava_id)?;
            Ok(Some((athlete.clone(), activity)))
        })
    })
    .await
    .map_err(|e| {
        log::error!("DB join error: {}", e);
        to_request_error(e)
    })?
    .map_err(|e| {
        log::error!("DB error: {}", e);
        to_request_error(e)
    })?;

    match result {
        Some((athlete, Some(activity))) => {
            let notif = db2
                .build_notification(&athlete.name, &activity)
                .map_err(to_request_error)?;
            bot.send_message(msg.chat.id, notif.text).await?;
            bot.send_photo(msg.chat.id, InputFile::memory(notif.card_png))
                .caption(notif.caption)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
        }
        Some((_, None)) => {
            bot.send_message(
                msg.chat.id,
                format!("No cached activities found for '{}'.", name),
            )
            .await?;
        }
        None => {
            bot.send_message(msg.chat.id, format!("Athlete '{}' not found.", name))
                .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    #[test]
    fn test_admin_check_logic() {
        let config = Config {
            bot_admin_usernames: vec!["alice".into(), "bob".into()],
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            strava_apps: crate::config::StravaApps {
                slot_1: crate::config::StravaApp {
                    id: String::new(),
                    secret: String::new(),
                },
                slot_2: None,
            },
            poll_interval_seconds: 300,
            cold_start_lookback_days: 30,
            database_path: String::new(),
            tracked_activity_types: vec![],
            notification_mode: crate::config::NotificationMode::CardAndText,
        };

        assert!(config.bot_admin_usernames.iter().any(|a| a == "alice"));
        assert!(config.bot_admin_usernames.iter().any(|a| a == "bob"));
        assert!(!config.bot_admin_usernames.iter().any(|a| a == "charlie"));
    }

    fn dummy_config(slot_2_configured: bool) -> Config {
        Config {
            bot_admin_usernames: vec![],
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            strava_apps: crate::config::StravaApps {
                slot_1: crate::config::StravaApp {
                    id: "id1".into(),
                    secret: "sec1".into(),
                },
                slot_2: slot_2_configured.then(|| crate::config::StravaApp {
                    id: "id2".into(),
                    secret: "sec2".into(),
                }),
            },
            poll_interval_seconds: 300,
            cold_start_lookback_days: 30,
            database_path: String::new(),
            tracked_activity_types: vec![],
            notification_mode: crate::config::NotificationMode::CardAndText,
        }
    }

    fn open_test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap()
    }

    #[test]
    fn resolve_slot_empty_db_returns_one() {
        let db = open_test_db();
        let cfg = dummy_config(true);
        db.run(|conn| {
            let slot = resolve_slot(conn, 111, &cfg).unwrap().unwrap();
            assert_eq!(slot, Slot::One);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn resolve_slot_one_taken_returns_two() {
        let db = open_test_db();
        let cfg = dummy_config(true);
        db.run(|conn| {
            db::insert_athlete(conn, 1, "alice", "a", "r", 0, Slot::One).unwrap();
            let slot = resolve_slot(conn, 222, &cfg).unwrap().unwrap();
            assert_eq!(slot, Slot::Two);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn resolve_slot_one_taken_app2_unconfigured_errors() {
        let db = open_test_db();
        let cfg = dummy_config(false);
        db.run(|conn| {
            db::insert_athlete(conn, 1, "alice", "a", "r", 0, Slot::One).unwrap();
            let err = resolve_slot(conn, 222, &cfg).unwrap().unwrap_err();
            assert_eq!(err, SlotError::Slot1FullApp2NotConfigured);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn resolve_slot_both_taken_errors() {
        let db = open_test_db();
        let cfg = dummy_config(true);
        db.run(|conn| {
            db::insert_athlete(conn, 1, "alice", "a", "r", 0, Slot::One).unwrap();
            db::insert_athlete(conn, 2, "bob", "a", "r", 0, Slot::Two).unwrap();
            let err = resolve_slot(conn, 333, &cfg).unwrap().unwrap_err();
            assert_eq!(err, SlotError::BothFull);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn resolve_slot_reuses_existing_athlete_slot() {
        let db = open_test_db();
        let cfg = dummy_config(true);
        db.run(|conn| {
            db::insert_athlete(conn, 1, "alice", "a", "r", 0, Slot::One).unwrap();
            db::insert_athlete(conn, 2, "bob", "a", "r", 0, Slot::Two).unwrap();
            // Re-auth for bob → keep his slot 2 even though both are full.
            let slot = resolve_slot(conn, 2, &cfg).unwrap().unwrap();
            assert_eq!(slot, Slot::Two);
            Ok(())
        })
        .unwrap();
    }
}
