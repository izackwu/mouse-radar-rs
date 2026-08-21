use anyhow::Result;
use chrono::Utc;
use log::{error, info, warn};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{InputFile, ParseMode};

use tokio::sync::mpsc::UnboundedReceiver;

use crate::ai::AiClient;
use crate::config::{Config, NotificationMode};
use crate::db::{self, CachedActivity, Db};
use crate::strava::{to_cached, StravaActivity, StravaApi};
use crate::types::ActivityType;

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
    ai: Option<&Arc<dyn AiClient>>,
) -> Result<()> {
    let chat_id: ChatId = ChatId(config.telegram_chat_id.parse()?);
    let athletes = {
        let db = Arc::clone(db);
        tokio::task::spawn_blocking(move || db.run(db::list_athletes)).await??
    };

    if athletes.is_empty() {
        return Ok(());
    }

    let tracked = config.tracked_activity_types.clone();
    let lookback = chrono::Duration::days(config.cold_start_lookback_days);

    for athlete in &athletes {
        if let Err(e) = process_athlete(
            config, db, strava, bot, &chat_id, athlete, &tracked, lookback, ai,
        )
        .await
        {
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
    ai: Option<Arc<dyn AiClient>>,
) {
    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(config.poll_interval_seconds));
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
                if let Err(e) = run_poll_cycle(&config, &db, &strava, &bot, ai.as_ref()).await {
                    error!("Poll cycle failed: {}", e);
                }
            }
            Some(cmd) = rx.recv() => {
                match cmd {
                    PollCommand::PollAll => {
                        info!("Starting poll cycle (triggered)...");
                        if let Err(e) = run_poll_cycle(&config, &db, &strava, &bot, ai.as_ref()).await {
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
                            &athlete, &tracked, lookback, ai.as_ref(),
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
    config: &Config,
    db: &Arc<Db>,
    strava: &Arc<dyn StravaApi>,
    bot: &Bot,
    chat_id: &ChatId,
    athlete: &db::Athlete,
    tracked_types: &[ActivityType],
    lookback: chrono::Duration,
    ai: Option<&Arc<dyn AiClient>>,
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
                })
                .await??;
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

    // 2. Determine the `after` epoch for the API call.
    // Must use the true-UTC start_date: start_date_local is mislabeled `Z` by
    // Strava, so deriving the cutoff from it would jump hours into the future in
    // positive-offset zones and drop same-day activities after the first.
    let db_clone = Arc::clone(db);
    let strava_id = athlete.strava_id;
    let (last_utc, has_cached): (Option<String>, bool) = tokio::task::spawn_blocking(move || {
        db_clone.run(|conn| {
            Ok((
                db::get_last_activity_utc(conn, strava_id)?,
                db::has_cached_activities(conn, strava_id)?,
            ))
        })
    })
    .await??;

    // Cold start is "never cached anything for this athlete", not "no UTC cutoff":
    // existing athletes have legacy rows with no start_date, so last_utc is None
    // for them on the first poll after deploy — they must still be notified.
    let after_epoch = last_utc
        .as_ref()
        .and_then(|d| parse_iso_to_epoch(d))
        .unwrap_or_else(|| (Utc::now() - lookback).timestamp());

    let is_cold_start = !has_cached;

    // 3. Fetch activities
    let activities = strava
        .get_activities(&access_token, Some(after_epoch), None, 50)
        .await?;

    if activities.is_empty() {
        return Ok(());
    }

    // 4. Select new, tracked activities. Strava returns ascending order when
    // `after` is set, so we check every fetched activity's seen status rather
    // than breaking at the first seen one (which would skip newer activities
    // that sort after it).
    let ids: Vec<i64> = activities.iter().map(|a| a.id).collect();
    let db_clone = Arc::clone(db);
    let seen =
        tokio::task::spawn_blocking(move || db_clone.run(|conn| db::get_seen_ids(conn, &ids)))
            .await??;

    // Sorted oldest-first so notifications arrive chronologically.
    let new_activities = select_new_tracked(&activities, &seen, tracked_types);

    if new_activities.is_empty() {
        return Ok(());
    }

    // Mark all new activities as seen
    {
        let db_clone = Arc::clone(db);
        let items: Vec<(i64, i64)> = new_activities
            .iter()
            .map(|a| (a.activity_id, athlete.strava_id))
            .collect();
        let items_clone = items.clone();
        tokio::task::spawn_blocking(move || {
            db_clone.run(|conn| db::bulk_mark_seen(conn, &items_clone))
        })
        .await??;
    }

    // Cache tracked activities and send notifications (already oldest-first).
    for cached in &new_activities {
        // Cache the activity
        let db_clone = Arc::clone(db);
        let c = cached.clone();
        tokio::task::spawn_blocking(move || db_clone.run(|conn| db::cache_activity(conn, &c)))
            .await??;

        // Don't notify on cold start (seeding)
        if !is_cold_start {
            let db_clone = Arc::clone(db);
            let name = athlete.name.clone();
            let cached_clone = cached.clone();
            let notif = tokio::task::spawn_blocking(move || {
                db_clone.build_notification(&name, &cached_clone)
            })
            .await??;

            let send_text = matches!(
                config.notification_mode,
                NotificationMode::TextOnly | NotificationMode::CardAndText
            );
            let send_card = matches!(
                config.notification_mode,
                NotificationMode::CardOnly | NotificationMode::CardAndText
            );
            let mut delivered = false;
            let mut reply_to: Option<teloxide::types::MessageId> = None;

            if send_text {
                match bot.send_message(*chat_id, notif.text).await {
                    Ok(m) => {
                        delivered = true;
                        reply_to = Some(m.id);
                    }
                    Err(e) => error!("Failed to send message for {}: {}", athlete.name, e),
                }
            }
            if send_card {
                match bot
                    .send_photo(*chat_id, InputFile::memory(notif.card_png))
                    .caption(notif.caption)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await
                {
                    Ok(m) => {
                        delivered = true;
                        reply_to = Some(m.id);
                    }
                    Err(e) => error!("Failed to send card photo for {}: {}", athlete.name, e),
                }
            }
            if delivered {
                info!("Notified: {} - {}", athlete.name, cached.title);

                // Comment is best-effort and fully detached: the notification
                // above is already delivered and cannot be affected.
                if let (Some(ai), Some(reply_id), Some(ai_cfg)) = (ai, reply_to, config.ai.as_ref())
                {
                    crate::comment::spawn_comment(
                        Arc::clone(ai),
                        Arc::clone(strava),
                        Arc::clone(db),
                        bot.clone(),
                        *chat_id,
                        reply_id,
                        ai_cfg.clone(),
                        access_token.clone(),
                        athlete.name.clone(),
                        cached.clone(),
                    );
                }
            }
        }
    }

    Ok(())
}

/// Pick the new, tracked activities to cache/notify from a fetched batch.
///
/// Independent of Strava's ordering: when `after` is set, Strava returns
/// activities in ascending (oldest-first) order, so we must not stop at the
/// first already-seen one. Returns them sorted oldest-first by UTC start so
/// notifications arrive chronologically.
fn select_new_tracked(
    activities: &[StravaActivity],
    seen: &HashSet<i64>,
    tracked_types: &[ActivityType],
) -> Vec<CachedActivity> {
    let mut out: Vec<CachedActivity> = activities
        .iter()
        .filter(|a| !seen.contains(&a.id) && tracked_types.contains(&a.activity_type))
        .map(to_cached)
        .collect();
    out.sort_by(|a, b| a.start_date.cmp(&b.start_date));
    out
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
    #![allow(clippy::float_cmp)]
    use super::*;

    fn sa(id: i64, activity_type: ActivityType, start_date: &str) -> StravaActivity {
        StravaActivity {
            id,
            athlete: crate::strava::StravaAthleteSummary { id: 1 },
            name: "Activity".into(),
            activity_type,
            distance: 1000.0,
            moving_time: 300,
            elapsed_time: 300,
            start_date: start_date.into(),
            start_date_local: start_date.into(),
        }
    }

    #[test]
    fn test_select_new_tracked_does_not_break_on_seen_in_ascending_order() {
        // Strava returns ascending order when `after` is set, so already-seen
        // activities appear BEFORE the new one. The new one must still be picked.
        let seen: HashSet<i64> = [1, 2].into_iter().collect();
        let tracked = vec![ActivityType::Run];
        let ascending = vec![
            sa(1, ActivityType::Run, "2026-06-18T06:00:00Z"), // seen
            sa(2, ActivityType::Run, "2026-06-19T06:00:00Z"), // seen
            sa(3, ActivityType::Run, "2026-06-21T06:28:00Z"), // new
        ];

        let got: Vec<i64> = select_new_tracked(&ascending, &seen, &tracked)
            .iter()
            .map(|c| c.activity_id)
            .collect();
        assert_eq!(got, vec![3]);
    }

    #[test]
    fn test_select_new_tracked_filters_seen_and_untracked_and_sorts() {
        let seen: HashSet<i64> = [10].into_iter().collect();
        let tracked = vec![ActivityType::Run, ActivityType::Hike];
        // Newest-first input, mixed types, one already seen.
        let input = vec![
            sa(13, ActivityType::Run, "2026-06-22T06:00:00Z"),
            sa(12, ActivityType::Ride, "2026-06-21T06:00:00Z"), // untracked
            sa(11, ActivityType::Hike, "2026-06-20T06:00:00Z"),
            sa(10, ActivityType::Run, "2026-06-19T06:00:00Z"), // seen
        ];

        let got: Vec<i64> = select_new_tracked(&input, &seen, &tracked)
            .iter()
            .map(|c| c.activity_id)
            .collect();
        // 12 dropped (untracked), 10 dropped (seen); remainder sorted oldest-first.
        assert_eq!(got, vec![11, 13]);
    }

    #[test]
    fn test_parse_iso_to_epoch_valid() {
        let epoch = parse_iso_to_epoch("2024-01-15T08:30:00Z").unwrap();
        assert_eq!(epoch, 1_705_307_400);
    }

    #[test]
    fn test_parse_iso_to_epoch_invalid() {
        assert!(parse_iso_to_epoch("not-a-date").is_none());
    }
}
