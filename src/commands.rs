use log::info;
use std::sync::Arc;
use teloxide::{prelude::*, utils::command::BotCommands, RequestError};

use crate::config::Config;
use crate::db::{self, Db};
use crate::strava::TokenResponse;

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
}

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: Arc<Db>,
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
    }
}

async fn cmd_help(bot: Bot, msg: Message) -> ResponseResult<()> {
    bot.send_message(
        msg.chat.id,
        "/help — Show this help message\n\
         /list — List all tracked athletes\n\
         /strava <name> — Show stats for an athlete\n\
         /register <name> <strava_id> — Register a new athlete (admin only)\n\
         /auth <name> <strava_id> <code> — Authorize athlete with OAuth code (admin only)"
            .to_string(),
    )
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
            .map(|a| format!("• {} (Strava ID: {})", a.name, a.strava_id))
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

    let auth_url = format!(
        "https://www.strava.com/oauth/authorize?client_id={}&redirect_uri=http://localhost&response_type=code&approval_prompt=force&scope=read,activity:read,activity:read_all",
        state.config.strava_client_id
    );

    bot.send_message(
        msg.chat.id,
        format!(
            "Send this link to the athlete (Strava ID: {}):\n\n{}\n\n\
             Once authorized, use:\n/auth {} {} <code>",
            strava_id, auth_url, name, strava_id,
        ),
    )
    .await?;
    info!("Registration link generated for {} ({})", name, strava_id);
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

    // Exchange code for tokens
    let http = reqwest::Client::new();
    let resp = http
        .post("https://www.strava.com/oauth/token")
        .form(&[
            ("client_id", state.config.strava_client_id.as_str()),
            ("client_secret", state.config.strava_client_secret.as_str()),
            ("code", code.as_str()),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await;

    let token: TokenResponse = match resp {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(t) => t,
            Err(e) => {
                bot.send_message(msg.chat.id, format!("Failed to parse token: {}", e))
                    .await?;
                return Ok(());
            }
        },
        Ok(r) => {
            let body = r.text().await.unwrap_or_default();
            bot.send_message(msg.chat.id, format!("Strava OAuth failed: {}", body))
                .await?;
            return Ok(());
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("Request error: {}", e))
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
        db.run(|conn| db::insert_athlete(conn, strava_id_int, &n, &acc, &refr, exp))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_admin_check_logic() {
        let config = Config {
            bot_admin_usernames: vec!["alice".into(), "bob".into()],
            telegram_bot_token: "".into(),
            telegram_chat_id: "".into(),
            strava_client_id: "".into(),
            strava_client_secret: "".into(),
            poll_interval_seconds: 300,
            cold_start_lookback_days: 30,
            database_path: "".into(),
            tracked_activity_types: vec![],
        };

        assert!(config.bot_admin_usernames.iter().any(|a| a == "alice"));
        assert!(config.bot_admin_usernames.iter().any(|a| a == "bob"));
        assert!(!config.bot_admin_usernames.iter().any(|a| a == "charlie"));
    }
}
