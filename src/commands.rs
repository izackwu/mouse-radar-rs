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
use crate::strava::TokenResponse;

/// Convert a displayable error into a `RequestError` (for use in ? propagation).
fn to_request_error(e: impl std::fmt::Display) -> RequestError {
    RequestError::Io(std::sync::Arc::new(std::io::Error::other(e.to_string())))
}

// Doc comments here double as the bot's /help text, so no rustdoc backticks.
#[allow(clippy::doc_markdown)]
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    /// Show this help message
    Help,
    /// List all tracked athletes
    List,
    /// Show stats for an athlete. Usage: /strava <name>
    Strava(String),
    /// Register a new athlete (admin only). Usage: /register <name> <strava_id>
    #[command(parse_with = "split")]
    Register { name: String, strava_id: i64 },
    /// Authorize an athlete with OAuth code (admin only). Usage: /auth <name> <strava_id> <code>
    #[command(parse_with = "split")]
    Auth {
        name: String,
        strava_id: i64,
        code: String,
    },
    /// Show the latest activity for an athlete with card image. Usage: /latest <name>
    Latest(String),
    /// Run an athlete's latest activity through the AI commenter (admin only). Usage: /aicomment <name>
    Aicomment(String),
}

/// Usage line for a message that looks like one of our commands but failed to
/// parse (wrong/missing arguments). Returns `None` for anything else so
/// unrelated chat messages and other bots' commands stay ignored.
#[must_use]
pub fn usage_for(text: &str, bot_username: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    let cmd = first.strip_prefix('/')?;
    let (name, mention) = match cmd.split_once('@') {
        Some((n, m)) => (n, Some(m)),
        None => (cmd, None),
    };
    if mention.is_some_and(|m| !m.eq_ignore_ascii_case(bot_username)) {
        return None;
    }
    Command::bot_commands()
        .into_iter()
        .find(|c| c.command.trim_start_matches('/') == name)
        .map(|c| c.description)
}

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: Arc<Db>,
    pub poll_tx: tokio::sync::mpsc::UnboundedSender<crate::poller::PollCommand>,
    /// Shared with the poller so `/aicomment` runs the same client production
    /// does. `None` when `AI_API_KEY` is unset.
    pub ai: Option<Arc<dyn crate::ai::AiClient>>,
    pub strava: Arc<dyn crate::strava::StravaApi>,
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
        Command::Register { name, strava_id } => {
            cmd_register(bot, msg, name, strava_id, state).await
        }
        Command::Auth {
            name,
            strava_id,
            code,
        } => cmd_auth(bot, msg, name, strava_id, code, state).await,
        Command::Latest(name) => cmd_latest(bot, msg, name, state).await,
        Command::Aicomment(name) => cmd_aicomment(bot, msg, name, state).await,
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
    strava_id: i64,
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
    strava_id: i64,
    code: String,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    if !is_admin(&msg, &state.config) {
        bot.send_message(msg.chat.id, "This command is restricted to admin users.")
            .await?;
        return Ok(());
    }

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
        db.run(|conn| db::upsert_athlete(conn, strava_id, &n, &acc, &refr, exp))
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

    info!("Athlete {} ({}) authorized", name, strava_id);

    // Notify the poller to cold-start this athlete immediately
    let _ = state
        .poll_tx
        .send(crate::poller::PollCommand::ColdStart(strava_id));

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

/// Telegram rejects messages over 4096 characters. Leave headroom for the
/// chunk header we prepend.
const TELEGRAM_CHUNK_CHARS: usize = 3900;

/// Run an athlete's latest cached activity through the real comment path and
/// reply with both the comment and the prompt that produced it.
///
/// Admin-only, and deliberately routed through `comment::compose_comment` —
/// the same function the poller uses — so what this prints is what production
/// would produce, not a parallel debug implementation.
async fn cmd_aicomment(
    bot: Bot,
    msg: Message,
    name: String,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    if !is_admin(&msg, &state.config) {
        bot.send_message(msg.chat.id, "This command is restricted to admin users.")
            .await?;
        return Ok(());
    }

    let (Some(ai), Some(ai_cfg)) = (state.ai.as_ref(), state.config.ai.as_ref()) else {
        bot.send_message(
            msg.chat.id,
            "AI comments are disabled (AI_API_KEY not set).",
        )
        .await?;
        return Ok(());
    };

    // Resolve the athlete and their latest cached activity in one DB hop.
    let db = Arc::clone(&state.db);
    let name_clone = name.clone();
    let found = tokio::task::spawn_blocking(move || {
        db.run(|conn| {
            let athletes = db::list_athletes(conn)?;
            let Some(athlete) = athletes
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case(&name_clone))
            else {
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

    let (athlete, activity) = match found {
        Some((athlete, Some(activity))) => (athlete, activity),
        Some((_, None)) => {
            bot.send_message(
                msg.chat.id,
                format!("No cached activities found for '{}'.", name),
            )
            .await?;
            return Ok(());
        }
        None => {
            bot.send_message(msg.chat.id, format!("Athlete '{}' not found.", name))
                .await?;
            return Ok(());
        }
    };

    // Note: unlike the poller, this path does not refresh an expiring token.
    // A stale token makes the detail fetch fail, which degrades the prompt
    // rather than failing it — `had_detail` below reports when that happened.
    let composed = match crate::comment::compose_comment(
        ai,
        &state.strava,
        &state.db,
        ai_cfg,
        &athlete.access_token,
        &athlete.name,
        &activity,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            log::error!("AI comment failed for {}: {}", athlete.name, e);
            bot.send_message(msg.chat.id, format!("AI comment failed: {}", e))
                .await?;
            return Ok(());
        }
    };

    let Some(prompt) = composed.prompt else {
        bot.send_message(
            msg.chat.id,
            format!(
                "Skipped: no prior activities for '{}' to compare against.",
                athlete.name
            ),
        )
        .await?;
        return Ok(());
    };

    // Plain text throughout — model output contains characters MarkdownV2
    // would reject, and a rejected message would be silently lost.
    let header = if composed.had_detail {
        String::new()
    } else {
        "\n\n⚠️ Strava activity detail was unavailable (stale token?), so the \
         prompt has no splits or laps."
            .to_string()
    };
    let comment = composed
        .comment
        .unwrap_or_else(|| "(model returned nothing usable)".to_string());
    bot.send_message(msg.chat.id, format!("🤖 {}{}", comment, header))
        .await?;

    let chunks = crate::comment::chunk_text(&prompt, TELEGRAM_CHUNK_CHARS);
    let total = chunks.len();
    for (i, chunk) in chunks.into_iter().enumerate() {
        bot.send_message(
            msg.chat.id,
            format!("─── prompt ({}/{}) ───\n{}", i + 1, total, chunk),
        )
        .await?;
    }

    info!("/aicomment run for {} by admin", athlete.name);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    #[test]
    fn test_parse_register_typed() {
        let cmd = Command::parse("/register zack 96951505", "testbot").unwrap();
        match cmd {
            Command::Register { name, strava_id } => {
                assert_eq!(name, "zack");
                assert_eq!(strava_id, 96_951_505);
            }
            _ => panic!("expected Register"),
        }
    }

    #[test]
    fn test_parse_auth_typed() {
        let cmd = Command::parse("/auth zack 96951505 abc123", "testbot").unwrap();
        match cmd {
            Command::Auth {
                name,
                strava_id,
                code,
            } => {
                assert_eq!(name, "zack");
                assert_eq!(strava_id, 96_951_505);
                assert_eq!(code, "abc123");
            }
            _ => panic!("expected Auth"),
        }
    }

    #[test]
    fn test_parse_aicomment() {
        let cmd = Command::parse("/aicomment zack", "testbot").unwrap();
        match cmd {
            Command::Aicomment(name) => assert_eq!(name, "zack"),
            _ => panic!("expected Aicomment"),
        }
    }

    #[test]
    fn test_aicomment_usage_hint_marks_it_admin_only() {
        // The doc comment doubles as /help text and as the malformed-command
        // usage reply, so the admin restriction must be visible there.
        let usage = usage_for("/aicomment", "testbot").expect("known command");
        assert!(usage.contains("admin only"), "usage was: {}", usage);
        assert!(usage.contains("/aicomment <name>"), "usage was: {}", usage);
    }

    #[test]
    fn test_parse_rejects_missing_or_invalid_args() {
        assert!(Command::parse("/register", "testbot").is_err());
        assert!(Command::parse("/register zack", "testbot").is_err());
        assert!(Command::parse("/register zack notanumber", "testbot").is_err());
        assert!(Command::parse("/auth zack 96951505", "testbot").is_err());
    }

    #[test]
    fn test_help_includes_usage() {
        let help = Command::descriptions().to_string();
        assert!(help.contains("/register <name> <strava_id>"));
        assert!(help.contains("/auth <name> <strava_id> <code>"));
        assert!(help.contains("/strava <name>"));
        assert!(help.contains("/latest <name>"));
    }

    #[test]
    fn test_usage_for_malformed_command() {
        let usage = usage_for("/register zack", "testbot").unwrap();
        assert!(usage.contains("/register <name> <strava_id>"));

        let usage = usage_for("/auth@testbot zack", "testbot").unwrap();
        assert!(usage.contains("/auth <name> <strava_id> <code>"));
    }

    #[test]
    fn test_usage_for_ignores_other_text_and_bots() {
        assert!(usage_for("hello world", "testbot").is_none());
        assert!(usage_for("/unknowncmd foo", "testbot").is_none());
        assert!(usage_for("/register@otherbot zack", "testbot").is_none());
    }

    #[test]
    fn test_admin_check_logic() {
        let config = Config {
            bot_admin_usernames: vec!["alice".into(), "bob".into()],
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            strava_client_id: String::new(),
            strava_client_secret: String::new(),
            poll_interval_seconds: 300,
            cold_start_lookback_days: 30,
            database_path: String::new(),
            tracked_activity_types: vec![],
            notification_mode: crate::config::NotificationMode::CardAndText,
            ai: None,
        };

        assert!(config.bot_admin_usernames.iter().any(|a| a == "alice"));
        assert!(config.bot_admin_usernames.iter().any(|a| a == "bob"));
        assert!(!config.bot_admin_usernames.iter().any(|a| a == "charlie"));
    }
}
