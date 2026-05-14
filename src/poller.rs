use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use chrono::Utc;
use log::{info, warn, error};
use teloxide::prelude::*;

use tokio::sync::mpsc::UnboundedReceiver;

use crate::config::Config;
use crate::db::{self, CachedActivity, Db};
use crate::strava::{StravaApi, to_cached};

pub enum PollCommand {
    PollAll,
    ColdStart(i64), // strava_id
}

/// Run a single poll cycle for all athletes. Called from the interval loop.
pub async fn run_poll_cycle(
    config: &Config,
    db: &Arc<Db>,
    strava: &Arc<dyn StravaApi>,
    bot: &Bot,
) -> Result<()> {
    let chat_id: ChatId = ChatId(config.telegram_chat_id.parse()?);
    let athletes = {
        let db = Arc::clone(db);
        tokio::task::spawn_blocking(move || db.run(db::list_athletes))
            .await??
    };

    if athletes.is_empty() {
        return Ok(());
    }

    let tracked: Vec<String> = config.tracked_activity_types.clone();
    let lookback = chrono::Duration::days(config.cold_start_lookback_days);

    for athlete in &athletes {
        if let Err(e) = process_athlete(
            config, db, strava, bot, &chat_id, athlete,
            &tracked, lookback,
        ).await {
            error!(
                "Error processing athlete {} ({}): {}",
                athlete.name, athlete.strava_id, e
            );
        }

        // 1-second delay between athletes to be gentle on Strava API
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    Ok(())
}

/// Run the poll loop: timer-driven full cycles + channel-driven cold starts.
pub async fn run_poll_loop(
    config: Config,
    db: Arc<Db>,
    strava: Arc<dyn StravaApi>,
    bot: Bot,
    mut rx: UnboundedReceiver<PollCommand>,
) {
    let mut interval = tokio::time::interval(
        std::time::Duration::from_secs(config.poll_interval_seconds),
    );
    // Skip immediate first tick
    interval.tick().await;

    let chat_id = match config.telegram_chat_id.parse::<i64>() {
        Ok(id) => ChatId(id),
        Err(e) => {
            error!("Invalid chat ID: {}", e);
            return;
        }
    };
    let tracked = config.tracked_activity_types.clone();
    let lookback = chrono::Duration::days(config.cold_start_lookback_days);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                info!("Starting poll cycle...");
                if let Err(e) = run_poll_cycle(&config, &db, &strava, &bot).await {
                    error!("Poll cycle failed: {}", e);
                }
            }
            Some(cmd) = rx.recv() => {
                match cmd {
                    PollCommand::PollAll => {
                        info!("Starting poll cycle (triggered)...");
                        if let Err(e) = run_poll_cycle(&config, &db, &strava, &bot).await {
                            error!("Poll cycle failed: {}", e);
                        }
                    }
                    PollCommand::ColdStart(strava_id) => {
                        info!("Cold-starting athlete {}", strava_id);
                        // Read athlete from DB to get current tokens
                        let athlete = {
                            let db = Arc::clone(&db);
                            match tokio::task::spawn_blocking(move || {
                                db.run(|conn| db::get_athlete(conn, strava_id))
                            }).await {
                                Ok(Ok(Some(a))) => a,
                                Ok(Ok(None)) => {
                                    error!("Athlete {} not found for cold start", strava_id);
                                    continue;
                                }
                                Err(e) => {
                                    error!("DB error reading athlete {}: {}", strava_id, e);
                                    continue;
                                }
                                _ => {
                                    error!("Unexpected DB error for athlete {}", strava_id);
                                    continue;
                                }
                            }
                        };
                        if let Err(e) = process_athlete(
                            &config, &db, &strava, &bot, &chat_id,
                            &athlete, &tracked, lookback,
                        ).await {
                            error!("Cold start for {} failed: {}", athlete.name, e);
                        } else {
                            info!("Cold start for {} complete", athlete.name);
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn process_athlete(
    _config: &Config,
    db: &Arc<Db>,
    strava: &Arc<dyn StravaApi>,
    bot: &Bot,
    chat_id: &ChatId,
    athlete: &db::Athlete,
    tracked_types: &[String],
    lookback: chrono::Duration,
) -> Result<()> {
    // 1. Refresh token if expiring within 1 hour
    let now = Utc::now().timestamp();
    let mut access_token = athlete.access_token.clone();
    let mut refresh_token = athlete.refresh_token.clone();
    let mut token_expires = athlete.token_expires;

    if token_expires - now < 3600 {
        info!("Refreshing token for {} (expiring soon)", athlete.name);
        match strava.refresh_token(&refresh_token).await {
            Ok(resp) => {
                access_token = resp.access_token.clone();
                refresh_token = resp.refresh_token.clone();
                token_expires = resp.expires_at;

                let db = Arc::clone(db);
                let strava_id = athlete.strava_id;
                let acc = access_token.clone();
                let refr = refresh_token.clone();
                tokio::task::spawn_blocking(move || {
                    db.run(|conn| {
                        db::update_athlete_tokens(conn, strava_id, &acc, &refr, token_expires)
                    })
                }).await??;
            }
            Err(e) => {
                warn!(
                    "Token refresh failed for {} ({}): {}. Skipping this cycle.",
                    athlete.name, athlete.strava_id, e
                );
                return Ok(());
            }
        }
    }

    // 2. Determine the `after` epoch for the API call
    let db_clone = Arc::clone(db);
    let strava_id = athlete.strava_id;
    let last_date: Option<String> = tokio::task::spawn_blocking(move || {
        db_clone.run(|conn| db::get_last_activity_date(conn, strava_id))
    }).await??;

    let after_epoch = last_date
        .as_ref()
        .and_then(|d| parse_iso_to_epoch(d))
        .unwrap_or_else(|| {
            (Utc::now() - lookback).timestamp()
        });

    let is_cold_start = last_date.is_none();

    // 3. Fetch activities
    let activities = strava.get_activities(
        &access_token,
        Some(after_epoch),
        None,
        50,
    ).await?;

    if activities.is_empty() {
        return Ok(());
    }

    // 4. Process each activity (newest first from Strava)
    let mut new_activities: Vec<CachedActivity> = Vec::new();

    for activity in &activities {
        let db_clone = Arc::clone(db);
        let activity_id = activity.id;
        let seen = tokio::task::spawn_blocking(move || {
            db_clone.run(|conn| db::is_activity_seen(conn, activity_id))
        }).await??;

        if seen {
            // Already seen => all older activities also seen (newest-first ordering)
            break;
        }

        let cached = to_cached(activity);

        // Skip non-tracked types for caching, but mark as seen
        if tracked_types.contains(&cached.activity_type) {
            new_activities.push(cached);
        }
    }

    if new_activities.is_empty() {
        return Ok(());
    }

    // Mark all new activities as seen
    {
        let db_clone = Arc::clone(db);
        let items: Vec<(i64, i64)> = new_activities.iter()
            .map(|a| (a.activity_id, athlete.strava_id))
            .collect();
        let items_clone = items.clone();
        tokio::task::spawn_blocking(move || {
            db_clone.run(|conn| db::bulk_mark_seen(conn, &items_clone))
        }).await??;
    }

    // Cache tracked activities and send notifications
    // Process from oldest to newest so notifications arrive chronologically
    new_activities.reverse();

    for cached in &new_activities {
        // Cache the activity
        let db_clone = Arc::clone(db);
        let c = cached.clone();
        tokio::task::spawn_blocking(move || {
            db_clone.run(|conn| db::cache_activity(conn, &c))
        }).await??;

        // Don't notify on cold start (seeding)
        if !is_cold_start {
            // Query stats
            let db_clone = Arc::clone(db);
            let sid = athlete.strava_id;
            let (monday, first_of_month) = db::period_boundaries();
            let (week_km, month_km, oldest_date) = tokio::task::spawn_blocking(move || {
                db_clone.run(|conn| {
                    let w = db::get_week_km(conn, sid, monday)?;
                    let m = db::get_month_km(conn, sid, first_of_month)?;
                    let o = db::get_oldest_activity_date(conn, sid)?;
                    Ok((w, m, o))
                })
            }).await??;

            let (incomplete_week, incomplete_month) = crate::formatting::incomplete_periods(
                oldest_date,
            );

            let msg = crate::formatting::format_activity_message(
                &athlete.name,
                &cached.title,
                &cached.activity_type,
                cached.distance_km,
                cached.pace_sec_per_km,
                cached.duration_s,
                week_km,
                month_km,
                &cached.url,
                incomplete_week,
                incomplete_month,
            );

            if let Err(e) = bot.send_message(*chat_id, msg).await {
                error!("Failed to send notification for {}: {}", athlete.name, e);
            } else {
                info!("Notified: {} - {}", athlete.name, cached.title);
            }
        }
    }

    Ok(())
}

fn parse_iso_to_epoch(iso: &str) -> Option<i64> {
    // Try RFC 3339 first (e.g. "2024-01-15T08:30:00Z")
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        return Some(dt.timestamp());
    }
    // Fallback: NaiveDateTime with Z as literal
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%SZ") {
        return Some(dt.and_utc().timestamp());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_iso_to_epoch_valid() {
        let epoch = parse_iso_to_epoch("2024-01-15T08:30:00Z").unwrap();
        assert_eq!(epoch, 1705307400);
    }

    #[test]
    fn test_parse_iso_to_epoch_invalid() {
        assert!(parse_iso_to_epoch("not-a-date").is_none());
    }
}
