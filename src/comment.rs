use std::sync::Arc;

use anyhow::Result;
use chrono::{Local, NaiveDate};
use log::{debug, info, warn};
use teloxide::prelude::*;
use teloxide::types::{MessageId, ReplyParameters};

use crate::ai::AiClient;
use crate::config::AiConfig;
use crate::db::{self, CachedActivity, Db};
use crate::formatting::{format_duration, format_pace};
use crate::strava::{StravaActivityDetail, StravaApi};
use crate::types::ActivityType;

/// Persona and guardrails for the comment. Overridable by pointing
/// `AI_SYSTEM_PROMPT_FILE` at a file.
///
/// The qualitative-comparison rule is deliberate: the model is handed a raw
/// history table and does its own arithmetic, so the prompt biases it toward
/// claims that stay true under approximation. The VOLUME block is the
/// exception — those totals are computed in `format_volume_block`, so the
/// model is told it may quote them directly.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are a running coach commenting in a group chat where friends' Strava \
activities are posted. Write ONE observation about the activity just posted, \
in at most two sentences.

- Address the athlete as \"you\".
- Ground the observation in their recent history: how this run compares to \
what they've been doing lately.
- Prefer qualitative comparisons (\"noticeably quicker than your recent easy \
runs\") over precise figures you'd have to calculate. If you do cite a number, \
it must appear verbatim in the data.
- The VOLUME totals are already calculated for you, so you may quote them \
directly. A figure marked \"(partial)\" is a floor, not a total — never call \
it a complete week or month, and never read a trend into it.
- Observe what happened. Do not prescribe future workouts.
- If the history is thin, keep it general — never invent a trend.
- Plain text only: no markdown, no emoji, no greeting or sign-off.";

/// History length at or below which the model is told not to infer trends.
const THIN_HISTORY_THRESHOLD: usize = 2;

/// Training volume over calendar periods, on the same Monday-start /
/// first-of-month boundaries the activity card uses — so the "this week"
/// figure in the prompt matches the WEEK figure on the card the comment
/// replies to.
///
/// Built by `db::collect_volume`. Distances sum every cached activity type,
/// not just runs, exactly as the card's totals do.
#[derive(Debug, Clone)]
pub struct VolumeStats {
    pub week_start: NaiveDate,
    pub week_km: f64,
    pub month_start: NaiveDate,
    pub month_km: f64,
    pub last_month_start: NaiveDate,
    pub last_month_km: f64,
    /// Completed weeks before the current one, newest first. Weeks that end
    /// before the cache begins are absent rather than zero — see
    /// `db::collect_volume`.
    pub prior_weeks: Vec<(NaiveDate, f64)>,
    /// Date of the athlete's oldest cached activity, or `None` when nothing is
    /// cached. Any period starting before this is a floor, not a total.
    pub oldest_cached: Option<NaiveDate>,
}

impl VolumeStats {
    /// Whether a period beginning on `start` reaches back further than the
    /// cache does, making its total a lower bound.
    ///
    /// Same rule as `formatting::incomplete_periods`, which drives the card's
    /// asterisk.
    #[must_use]
    fn is_partial(&self, start: NaiveDate) -> bool {
        self.oldest_cached.is_none_or(|oldest| oldest > start)
    }
}

/// Load the system prompt for a single request.
///
/// Deliberately re-read per request rather than cached at startup: the prompt
/// is the knob you turn most while tuning the bot's voice, and re-reading
/// makes an edit take effect on the next activity instead of the next deploy.
/// The file is a few kilobytes and one request already costs a network
/// round-trip, so the read is free by comparison.
///
/// Every failure — missing, unreadable, blank — falls back to the built-in
/// prompt and warns. A bad path should flatten the bot's personality, never
/// cost it a comment.
async fn resolve_system_prompt(cfg: &AiConfig) -> String {
    let Some(path) = &cfg.system_prompt_path else {
        return DEFAULT_SYSTEM_PROMPT.to_string();
    };

    match tokio::fs::read_to_string(path).await {
        Ok(text) if !text.trim().is_empty() => {
            let text = text.trim().to_string();
            debug!(
                "Loaded system prompt from {} ({} chars)",
                path,
                text.chars().count()
            );
            text
        }
        Ok(_) => {
            warn!(
                "System prompt file {} is empty; falling back to the built-in prompt",
                path
            );
            DEFAULT_SYSTEM_PROMPT.to_string()
        }
        Err(e) => {
            warn!(
                "Cannot read system prompt file {} ({}); falling back to the built-in prompt",
                path, e
            );
            DEFAULT_SYSTEM_PROMPT.to_string()
        }
    }
}

fn pr_phrase(rank: i64) -> String {
    match rank {
        1 => "fastest ever".to_string(),
        2 => "2nd fastest ever".to_string(),
        3 => "3rd fastest ever".to_string(),
        n => format!("{}th fastest ever", n),
    }
}

/// Strava reports speed in m/s; the prompt wants seconds per kilometre.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn pace_from_speed(mps: Option<f64>) -> Option<i64> {
    mps.filter(|v| *v > 0.0)
        .map(|v| (1000.0 / v).round() as i64)
}

fn opt_pace(mps: Option<f64>) -> String {
    pace_from_speed(mps).map_or_else(|| "—".to_string(), format_pace)
}

fn opt_num(v: Option<f64>) -> String {
    v.map_or_else(|| "—".to_string(), |n| format!("{:.0}", n))
}

fn opt_signed(v: Option<f64>) -> String {
    v.map_or_else(|| "—".to_string(), |n| format!("{:+.0}", n))
}

/// Convert Strava's reported cadence into the units the prompt claims.
///
/// For foot sports Strava reports RPM — one revolution is one stride, i.e.
/// two steps — so a reported 90 is really 180 spm. Cycling cadence is genuine
/// pedal RPM and is passed through untouched.
fn cadence_for(activity_type: ActivityType, raw: f64) -> (f64, &'static str) {
    if activity_type.cadence_is_per_leg() {
        (raw * 2.0, "spm")
    } else {
        (raw, "rpm")
    }
}

/// Flatten an activity title for a single table cell.
///
/// Strava titles are free text and may contain newlines, which would break
/// the aligned history table the model reads.
fn flatten_title(title: &str) -> String {
    let flat: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Render a `CachedActivity.start_date_local` for the prompt as
/// `YYYY-MM-DD HH:MM local`, dropping Strava's misleading `Z` suffix.
///
/// `start_date_local` is local wall-clock time wearing a UTC costume (see
/// CLAUDE.md) — this reformats the string as-is, with no timezone
/// arithmetic, so the model does not read the `Z` and reason about it as
/// UTC. Falls back to the raw string for anything shorter than the expected
/// `YYYY-MM-DDTHH:MM...` prefix rather than panicking.
fn format_local_display(start_date_local: &str) -> String {
    match (start_date_local.get(..10), start_date_local.get(11..16)) {
        (Some(date), Some(time)) => format!("{} {} local", date, time),
        _ => start_date_local.to_string(),
    }
}

/// Render the VOLUME block: calendar-period totals plus the recent weekly
/// breakdown.
///
/// These totals are precomputed on purpose. The system prompt tells the model
/// not to do its own arithmetic over the history table, which would otherwise
/// leave it no honest way to say anything about training volume.
fn format_volume_block(v: &VolumeStats) -> String {
    // The poller caches an activity before notifying, so the activity being
    // commented on is already inside these totals. Saying so is not optional:
    // otherwise the model reads the week figure as the state *before* this
    // run and adds the two. It lives here rather than in the system prompt
    // because `AI_SYSTEM_PROMPT_FILE` can replace that wholesale, and the
    // caveat has to travel with the numbers it describes.
    let mut s = String::from("\nVOLUME (including this activity)\n");

    let mark = |partial: bool| if partial { "  (partial)" } else { "" };

    let rows = [
        (
            format!("This week (Mon {} to date)", v.week_start),
            v.week_km,
            v.is_partial(v.week_start),
        ),
        (
            format!("This month ({} to date)", v.month_start.format("%b %Y")),
            v.month_km,
            v.is_partial(v.month_start),
        ),
        (
            format!("Last month ({})", v.last_month_start.format("%b %Y")),
            v.last_month_km,
            v.is_partial(v.last_month_start),
        ),
    ];

    // Bind the label to a String and pad it here rather than inline: the
    // three labels differ in length and the model reads this as a table.
    let width = rows
        .iter()
        .map(|(l, ..)| l.chars().count())
        .max()
        .unwrap_or(0);
    for (label, km, partial) in &rows {
        s.push_str(&format!(
            "  {:<width$}  {:>7.1} km{}\n",
            label,
            km,
            mark(*partial),
            width = width
        ));
    }

    // Count what is actually listed: `collect_volume` drops weeks that end
    // before the cache begins, so this is not always four.
    if !v.prior_weeks.is_empty() {
        let n = v.prior_weeks.len();
        s.push_str(&format!(
            "\n  {} week{} before this one:\n",
            n,
            if n == 1 { "" } else { "s" }
        ));
        for (monday, km) in &v.prior_weeks {
            s.push_str(&format!(
                "    week of {}  {:>7.1} km{}\n",
                monday,
                km,
                mark(v.is_partial(*monday))
            ));
        }
    }

    let any_partial = rows.iter().any(|(.., partial)| *partial)
        || v.prior_weeks.iter().any(|(m, _)| v.is_partial(*m));
    if any_partial {
        match v.oldest_cached {
            Some(oldest) => s.push_str(&format!(
                "\n  (partial) = this athlete's recorded history only starts {}, \
                 so the figure is a floor, not a total.\n",
                oldest
            )),
            None => s.push_str("\n  (partial) = no recorded history for this athlete yet.\n"),
        }
    }

    s
}

/// Build the user half of the prompt: the activity, its detail, and the
/// athlete's recent history.
#[must_use]
pub fn build_user_message(
    athlete_name: &str,
    activity: &CachedActivity,
    detail: Option<&StravaActivityDetail>,
    history: &[CachedActivity],
    volume: &VolumeStats,
) -> String {
    let mut s = String::new();

    s.push_str(&format!("ATHLETE: {}\n\n", athlete_name));
    s.push_str("THIS ACTIVITY\n");
    s.push_str(&format!(
        "  {} \"{}\" — {}\n",
        activity.activity_type,
        activity.title,
        format_local_display(&activity.start_date_local)
    ));
    match activity.pace_sec_per_km {
        Some(p) => s.push_str(&format!(
            "  {:.1} km · {} · {}/km\n",
            activity.distance_km,
            format_duration(activity.duration_s),
            format_pace(p)
        )),
        None => s.push_str(&format!(
            "  {:.1} km · {}\n",
            activity.distance_km,
            format_duration(activity.duration_s)
        )),
    }

    if let Some(d) = detail {
        // Summary extras: only fields the device actually reported.
        let mut extras: Vec<String> = Vec::new();
        if let Some(e) = d.total_elevation_gain {
            extras.push(format!("elevation {:.0} m", e));
        }
        if let Some(hr) = d.average_heartrate {
            extras.push(format!("avg HR {:.0}", hr));
        }
        if let Some(hr) = d.max_heartrate {
            extras.push(format!("max HR {:.0}", hr));
        }
        if let Some(c) = d.average_cadence {
            let (value, unit) = cadence_for(activity.activity_type, c);
            extras.push(format!("cadence {:.0} {}", value, unit));
        }
        if !extras.is_empty() {
            s.push_str(&format!("  {}\n", extras.join(" · ")));
        }

        // Only PR-ranked efforts: Strava's arithmetic, not the model's.
        for be in &d.best_efforts {
            if let Some(rank) = be.pr_rank {
                s.push_str(&format!(
                    "  {} in {} — {} (per Strava)\n",
                    be.name,
                    format_duration(be.elapsed_time),
                    pr_phrase(rank)
                ));
            }
        }

        if !d.splits_metric.is_empty() {
            s.push_str("\n  SPLITS (per km)  pace   avgHR   elev\n");
            for sp in &d.splits_metric {
                s.push_str(&format!(
                    "    {:<3} {:>13}  {:>5}  {:>5}\n",
                    sp.split,
                    opt_pace(sp.average_speed),
                    opt_num(sp.average_heartrate),
                    opt_signed(sp.elevation_difference),
                ));
            }
        }

        if !d.laps.is_empty() {
            let cad_unit = if activity.activity_type.cadence_is_per_leg() {
                "spm"
            } else {
                "rpm"
            };
            s.push_str("\n  LAPS\n");
            s.push_str(&format!(
                "    #    dist      pace   avgHR  maxHR  cad({})  elev\n",
                cad_unit
            ));
            for lap in &d.laps {
                let cadence = lap
                    .average_cadence
                    .map(|c| cadence_for(activity.activity_type, c).0);
                s.push_str(&format!(
                    "    {:<4} {:>5.2} km  {:>5}  {:>5}  {:>5}  {:>8}  {:>5}\n",
                    lap.lap_index,
                    lap.distance / 1000.0,
                    opt_pace(lap.average_speed),
                    opt_num(lap.average_heartrate),
                    opt_num(lap.max_heartrate),
                    opt_num(cadence),
                    opt_signed(lap.total_elevation_gain),
                ));
            }
        }
    }

    s.push_str(&format_volume_block(volume));

    s.push_str(&format!(
        "\nRECENT HISTORY (newest first — {} activities)\n",
        history.len()
    ));
    s.push_str("  date        type       distance      pace   duration  title\n");
    for h in history {
        let date = h.start_date_local.get(..10).unwrap_or(&h.start_date_local);
        let pace = h
            .pace_sec_per_km
            .map_or_else(|| "—".to_string(), format_pace);
        // Bind to a String first: column padding (`{:<10}`) is only honoured
        // by Display impls that route through `f.pad`, which strum's derive
        // does not guarantee. `String`'s impl does.
        let kind = h.activity_type.to_string();
        s.push_str(&format!(
            "  {}  {:<10} {:>6.1} km  {:>7}  {:>8}  {}\n",
            date,
            kind,
            h.distance_km,
            pace,
            format_duration(h.duration_s),
            flatten_title(&h.title),
        ));
    }

    if history.len() <= THIN_HISTORY_THRESHOLD {
        let (count, verb) = if history.len() == 1 {
            ("1 prior activity".to_string(), "is")
        } else {
            (format!("{} prior activities", history.len()), "are")
        };
        s.push_str(&format!(
            "\nNOTE: only {} {} on record for this athlete.\n\
             Comment on this activity alone; do not describe trends.\n",
            count, verb
        ));
    }

    s
}

/// Trim, reject empties, and cap length at a word boundary.
///
/// Returns `None` when there is nothing worth sending.
#[must_use]
pub fn sanitize(raw: &str, max_chars: usize) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= max_chars {
        return Some(trimmed.to_string());
    }

    // Reserve one char for the ellipsis.
    let budget = max_chars.saturating_sub(1);
    let mut out = String::new();
    for word in trimmed.split_whitespace() {
        let sep = usize::from(!out.is_empty());
        if out.chars().count() + sep + word.chars().count() > budget {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }

    // A single word longer than the budget leaves `out` empty — hard-cut it.
    if out.is_empty() {
        out = trimmed.chars().take(budget).collect();
    }
    out.push('…');
    Some(out)
}

/// The result of composing a comment: the prompt that was built, and the
/// comment the model produced from it.
///
/// The prompt is carried out of the function so the `/test-ai-comment` admin
/// command can show what the model actually saw. Production only reads
/// `comment`.
#[derive(Debug, Clone)]
pub struct Composed {
    /// The user half of the prompt. `None` when composition stopped before a
    /// prompt was built — currently only when the athlete has no prior
    /// activities to compare against.
    pub prompt: Option<String>,
    /// The sanitized comment. `None` when composition was skipped, or when
    /// the model returned nothing usable.
    pub comment: Option<String>,
    /// Whether Strava activity detail (splits, laps, best efforts) made it
    /// into the prompt. False means the fetch failed and the prompt fell back
    /// to summary + history — useful to surface when testing, since a stale
    /// access token produces a thinner comment that can look like a bug.
    pub had_detail: bool,
}

/// Produce the comment text for an activity, or `None` if there is nothing
/// worth sending.
///
/// Deliberately does not send: keeping the send out of this function is what
/// makes the whole path testable without a Telegram fake.
///
/// Errors from the AI client propagate; the caller logs and drops them. The
/// notification has already been delivered by the time this runs, so no
/// failure here is user-visible.
pub async fn compose_comment(
    ai: &Arc<dyn AiClient>,
    strava: &Arc<dyn StravaApi>,
    db: &Arc<Db>,
    cfg: &AiConfig,
    access_token: &str,
    athlete_name: &str,
    activity: &CachedActivity,
) -> Result<Composed> {
    // History, excluding the activity being commented on — the poller caches
    // before notifying, so it is already in the table. Also bounded by
    // `before_local`: the poller's cache-then-notify writes for later
    // activities race this task on the blocking pool with no ordering
    // barrier, so `exclude_id` alone is not enough to keep a not-yet-notified
    // activity out of this history (see `get_recent_activities`).
    //
    // The volume totals ride along in the same blocking hop. They are
    // deliberately NOT bounded by `before_local`: the card in the notification
    // this comment replies to includes the activity being commented on, and a
    // volume figure that disagreed with the card would be the more visible
    // wrong. The residual risk is a back-to-back activity cached mid-compose
    // nudging the total up.
    let (history, volume) = {
        let db = Arc::clone(db);
        let athlete_id = activity.athlete_id;
        let exclude = activity.activity_id;
        let before_local = activity.start_date_local.clone();
        let limit = cfg.history_limit;
        tokio::task::spawn_blocking(move || {
            db.run(|conn| {
                let history = db::get_recent_activities(
                    conn,
                    athlete_id,
                    Some(exclude),
                    Some(&before_local),
                    limit,
                )?;
                let volume = db::collect_volume(conn, athlete_id, Local::now().date_naive())?;
                Ok((history, volume))
            })
        })
        .await??
    };

    if history.is_empty() {
        debug!(
            "No prior activities for {}; skipping AI comment",
            athlete_name
        );
        return Ok(Composed {
            prompt: None,
            comment: None,
            had_detail: false,
        });
    }

    // Best-effort: a missing detail degrades the prompt, it does not fail it.
    let detail = match strava
        .get_activity_detail(access_token, activity.activity_id)
        .await
    {
        Ok(d) => Some(d),
        Err(e) => {
            warn!(
                "Activity detail fetch failed for {} ({}): {}. Continuing without it.",
                athlete_name, activity.activity_id, e
            );
            None
        }
    };

    let user = build_user_message(athlete_name, activity, detail.as_ref(), &history, &volume);
    let system = resolve_system_prompt(cfg).await;

    debug!(
        "Composing comment for {}: {} history rows, detail={}, prompt {} chars",
        athlete_name,
        history.len(),
        detail.is_some(),
        user.chars().count()
    );

    let response = ai.comment(&system, &user).await?;
    let comment = sanitize(&response.content, cfg.max_chars);

    if comment.is_none() {
        warn!(
            "No usable comment for {} ({}): {}",
            athlete_name,
            activity.activity_id,
            response.diagnostics()
        );
    } else {
        info!(
            "Comment composed for {} ({})",
            athlete_name, activity.activity_id
        );
    }

    Ok(Composed {
        comment,
        had_detail: detail.is_some(),
        prompt: Some(user),
    })
}

/// Split text into chunks that each fit within Telegram's message limit.
///
/// Breaks on line boundaries so prompt tables stay readable. A single line
/// longer than `limit` is hard-split by character count rather than dropped.
#[must_use]
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.split_inclusive('\n') {
        // A single line that cannot fit anywhere: flush, then hard-split it.
        if line.chars().count() > limit {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            let mut rest: Vec<char> = line.chars().collect();
            while rest.len() > limit {
                let tail = rest.split_off(limit);
                chunks.push(rest.into_iter().collect());
                rest = tail;
            }
            current = rest.into_iter().collect();
            continue;
        }

        if current.chars().count() + line.chars().count() > limit {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Fire-and-forget: generate the comment and post it as a threaded reply.
///
/// Spawned after the notification has already been sent, so nothing here can
/// delay or break a notification. All failures are logged and dropped.
#[allow(clippy::too_many_arguments)]
pub fn spawn_comment(
    ai: Arc<dyn AiClient>,
    strava: Arc<dyn StravaApi>,
    db: Arc<Db>,
    bot: Bot,
    chat_id: ChatId,
    reply_to: MessageId,
    cfg: AiConfig,
    access_token: String,
    athlete_name: String,
    activity: CachedActivity,
) {
    tokio::spawn(async move {
        let text = match compose_comment(
            &ai,
            &strava,
            &db,
            &cfg,
            &access_token,
            &athlete_name,
            &activity,
        )
        .await
        {
            Ok(Composed {
                comment: Some(text),
                ..
            }) => text,
            Ok(_) => return,
            Err(e) => {
                warn!("AI comment failed for {}: {}", athlete_name, e);
                return;
            }
        };

        // Plain text, no parse_mode: the model emits '.', '-', '(' freely and
        // MarkdownV2 would reject nearly every message — silently, since we
        // swallow the error.
        if let Err(e) = bot
            .send_message(chat_id, text)
            .reply_parameters(ReplyParameters::new(reply_to))
            .await
        {
            warn!("Failed to send AI comment for {}: {}", athlete_name, e);
        } else {
            debug!("AI comment posted for {}", athlete_name);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strava::{StravaBestEffort, StravaLap, StravaSplit};
    use crate::types::ActivityType;

    fn act(id: i64, day: &str, km: f64, pace: Option<i64>) -> CachedActivity {
        CachedActivity {
            activity_id: id,
            athlete_id: 1,
            title: format!("Run {}", id),
            activity_type: ActivityType::Run,
            distance_km: km,
            duration_s: 3600,
            pace_sec_per_km: pace,
            start_date_local: format!("2026-08-{}T06:28:00Z", day),
            start_date: format!("2026-08-{}T06:28:00Z", day),
            url: "u".into(),
        }
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// Volume as of Thu 2026-09-03, with a cache deep enough that nothing is
    /// partial.
    fn vol() -> VolumeStats {
        VolumeStats {
            week_start: date(2026, 8, 31),
            week_km: 28.4,
            month_start: date(2026, 9, 1),
            month_km: 41.2,
            last_month_start: date(2026, 8, 1),
            last_month_km: 168.9,
            prior_weeks: vec![
                (date(2026, 8, 24), 51.0),
                (date(2026, 8, 17), 38.2),
                (date(2026, 8, 10), 44.6),
                (date(2026, 8, 3), 29.1),
            ],
            oldest_cached: Some(date(2026, 5, 1)),
        }
    }

    /// The first line of `text` containing `needle`.
    fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
        text.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line containing {:?} in:\n{}", needle, text))
    }

    #[test]
    fn test_volume_block_renders_each_period_and_prior_weeks() {
        let block = format_volume_block(&vol());

        assert!(block.contains("VOLUME"), "block was:\n{}", block);
        assert!(line_with(&block, "This week").contains("Mon 2026-08-31 to date"));
        assert!(line_with(&block, "This week").contains("28.4 km"));
        // The year is spelled out: in January "last month" is the year before.
        assert!(line_with(&block, "This month").contains("Sep 2026 to date"));
        assert!(line_with(&block, "This month").contains("41.2 km"));
        assert!(line_with(&block, "Last month").contains("Aug 2026"));
        assert!(line_with(&block, "Last month").contains("168.9 km"));

        assert!(line_with(&block, "2026-08-17").contains("38.2 km"));
        assert!(line_with(&block, "2026-08-03").contains("29.1 km"));
        assert!(!block.contains("partial"), "block was:\n{}", block);
    }

    #[test]
    fn test_volume_block_heading_says_it_includes_this_activity() {
        // The poller caches before notifying, so the activity being commented
        // on is inside these totals. Left unsaid, the model reads "28.4 km
        // this week" as the week *before* this run and adds the two.
        //
        // This belongs in the user message, not the system prompt:
        // AI_SYSTEM_PROMPT_FILE can replace the system prompt wholesale, and
        // the caveat has to travel with the numbers it describes.
        let block = format_volume_block(&vol());

        // On the VOLUME heading itself, ahead of any figure.
        let heading = line_with(&block, "VOLUME");
        assert_eq!(heading, "VOLUME (including this activity)");
    }

    #[test]
    fn test_volume_block_marks_only_the_periods_the_cache_cannot_cover() {
        // Cache starts mid-August: last month's total is a floor, but this
        // week and this month are fully covered and must not be hedged.
        let v = VolumeStats {
            oldest_cached: Some(date(2026, 8, 5)),
            ..vol()
        };
        let block = format_volume_block(&v);

        assert!(line_with(&block, "Last month").contains("partial"));
        assert!(!line_with(&block, "This week").contains("partial"));
        assert!(!line_with(&block, "This month").contains("partial"));
        // The note names the date the history starts, so the model can see
        // exactly how far the floor is from the truth.
        assert!(block.contains("2026-08-05"), "block was:\n{}", block);
    }

    #[test]
    fn test_volume_block_counts_the_weeks_it_actually_shows() {
        // `collect_volume` drops weeks that end before the cache begins, so
        // the heading must not promise four weeks when it lists two.
        let v = VolumeStats {
            prior_weeks: vec![(date(2026, 8, 24), 51.0), (date(2026, 8, 17), 38.2)],
            oldest_cached: Some(date(2026, 8, 19)),
            ..vol()
        };
        let block = format_volume_block(&v);

        assert!(block.contains("2 weeks before"), "block was:\n{}", block);
        assert!(!block.contains("4 weeks before"), "block was:\n{}", block);
    }

    #[test]
    fn test_volume_block_omits_the_week_table_when_nothing_is_knowable() {
        let v = VolumeStats {
            prior_weeks: vec![],
            oldest_cached: Some(date(2026, 8, 31)),
            ..vol()
        };
        let block = format_volume_block(&v);

        assert!(!block.contains("weeks before"), "block was:\n{}", block);
        assert!(!block.contains("week of"), "block was:\n{}", block);
        // The periods themselves still render.
        assert!(block.contains("This week"), "block was:\n{}", block);
    }

    fn cfg_with_prompt(path: Option<&str>) -> AiConfig {
        AiConfig {
            api_key: "k".into(),
            base_url: "http://localhost".into(),
            model: "m".into(),
            history_limit: 30,
            timeout_seconds: 20,
            max_chars: 280,
            max_tokens: 1000,
            system_prompt_path: path.map(String::from),
        }
    }

    #[tokio::test]
    async fn test_system_prompt_defaults_when_no_path_set() {
        let prompt = resolve_system_prompt(&cfg_with_prompt(None)).await;
        assert_eq!(prompt, DEFAULT_SYSTEM_PROMPT);
    }

    #[tokio::test]
    async fn test_system_prompt_read_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coach.txt");
        std::fs::write(&path, "  You are a laconic trail runner.\n\n").unwrap();

        let prompt = resolve_system_prompt(&cfg_with_prompt(Some(path.to_str().unwrap()))).await;
        // Trimmed: trailing newlines from an editor are not part of the prompt.
        assert_eq!(prompt, "You are a laconic trail runner.");
    }

    #[tokio::test]
    async fn test_system_prompt_is_reread_on_every_call() {
        // The whole reason the prompt is a path and not a string: editing the
        // file must retune the bot without a restart.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coach.txt");
        let cfg = cfg_with_prompt(Some(path.to_str().unwrap()));

        std::fs::write(&path, "first voice").unwrap();
        assert_eq!(resolve_system_prompt(&cfg).await, "first voice");

        std::fs::write(&path, "second voice").unwrap();
        assert_eq!(resolve_system_prompt(&cfg).await, "second voice");
    }

    #[tokio::test]
    async fn test_system_prompt_missing_file_falls_back_to_default() {
        // A typo'd path must cost personality, not the comment itself.
        let cfg = cfg_with_prompt(Some("/nonexistent/definitely/not/here.txt"));
        assert_eq!(resolve_system_prompt(&cfg).await, DEFAULT_SYSTEM_PROMPT);
    }

    #[tokio::test]
    async fn test_system_prompt_blank_file_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "   \n\t\n").unwrap();

        let prompt = resolve_system_prompt(&cfg_with_prompt(Some(path.to_str().unwrap()))).await;
        assert_eq!(prompt, DEFAULT_SYSTEM_PROMPT);
    }

    #[test]
    fn test_user_message_includes_activity_and_history() {
        let current = act(99, "16", 12.4, Some(312));
        let history = vec![
            act(98, "14", 8.0, Some(327)),
            act(97, "12", 10.2, Some(331)),
        ];
        let msg = build_user_message("Zack", &current, None, &history, &vol());

        assert!(msg.contains("ATHLETE: Zack"));
        assert!(msg.contains("12.4 km"));
        assert!(msg.contains("5:12/km"));
        assert!(msg.contains("RECENT HISTORY"));
        assert!(msg.contains("2026-08-14"));
        assert!(msg.contains("8.0 km"));
    }

    #[test]
    fn test_user_message_places_volume_between_the_activity_and_the_history() {
        let msg = build_user_message(
            "Zack",
            &act(99, "16", 12.4, Some(312)),
            None,
            &[act(98, "14", 8.0, None)],
            &vol(),
        );

        let volume = msg.find("VOLUME").expect("VOLUME block missing");
        let history = msg.find("RECENT HISTORY").expect("history missing");
        let activity = msg.find("THIS ACTIVITY").expect("activity missing");
        assert!(
            activity < volume && volume < history,
            "block order wrong in:\n{}",
            msg
        );
        assert!(msg.contains("168.9 km"), "msg was:\n{}", msg);
    }

    #[test]
    fn test_thin_history_hint_fires_at_two_or_fewer() {
        let current = act(99, "16", 12.4, Some(312));
        let thin = vec![act(98, "14", 8.0, Some(327))];
        assert!(build_user_message("Zack", &current, None, &thin, &vol())
            .contains("do not describe trends"));

        let thick: Vec<CachedActivity> =
            (1..=5).map(|i| act(90 + i, "10", 8.0, Some(320))).collect();
        assert!(!build_user_message("Zack", &current, None, &thick, &vol())
            .contains("do not describe trends"));
    }

    #[test]
    fn test_thin_history_hint_boundary_is_exactly_two() {
        // Pins THIN_HISTORY_THRESHOLD == 2 exactly: fires at len == 2, does
        // not fire at len == 3. The len == 1 / len == 5 cases above would
        // pass unchanged for a threshold of 1, 2, 3, or 4.
        let current = act(99, "16", 12.4, Some(312));

        let two: Vec<CachedActivity> = (1..=2).map(|i| act(90 + i, "10", 8.0, Some(320))).collect();
        assert!(build_user_message("Zack", &current, None, &two, &vol())
            .contains("do not describe trends"));

        let three: Vec<CachedActivity> =
            (1..=3).map(|i| act(90 + i, "10", 8.0, Some(320))).collect();
        assert!(!build_user_message("Zack", &current, None, &three, &vol())
            .contains("do not describe trends"));
    }

    #[test]
    fn test_zero_history_note_is_factually_correct() {
        // `build_user_message` is `pub` with no documented precondition
        // against empty history, even though `compose_comment` currently
        // early-returns before calling it with one. The note must not claim
        // "2 prior activities" when there are zero.
        let current = act(99, "16", 12.4, Some(312));
        let msg = build_user_message("Zack", &current, None, &[], &vol());

        assert!(msg.contains("RECENT HISTORY (newest first — 0 activities)"));
        assert!(msg.contains("0 prior activities are on record"));
        assert!(!msg.contains("2 prior activities"));
    }

    #[test]
    fn test_activity_start_date_rendered_as_local_no_z_suffix() {
        // Strava mislabels start_date_local with a trailing `Z`, which reads
        // as UTC. This is the one place that raw string reaches the model;
        // it must be reformatted as "YYYY-MM-DD HH:MM local" instead.
        let current = act(99, "16", 12.4, Some(312));
        let msg = build_user_message("Zack", &current, None, &[], &vol());

        assert!(msg.contains("2026-08-16 06:28 local"));
        assert!(!msg.contains("2026-08-16T06:28:00Z"));
    }

    #[test]
    fn test_format_local_display_handles_malformed_input_without_panicking() {
        assert_eq!(format_local_display(""), "");
        assert_eq!(format_local_display("short"), "short");
        assert_eq!(
            format_local_display("2026-08-16T06:28:00Z"),
            "2026-08-16 06:28 local"
        );
    }

    #[test]
    fn test_no_detail_omits_all_detail_blocks() {
        let msg = build_user_message(
            "Zack",
            &act(99, "16", 12.4, Some(312)),
            None,
            &[act(98, "14", 8.0, None)],
            &vol(),
        );
        assert!(!msg.contains("SPLITS"));
        assert!(!msg.contains("LAPS"));
        assert!(!msg.contains("avg HR"));
    }

    #[test]
    fn test_detail_omits_missing_fields_rather_than_blanking() {
        let detail = StravaActivityDetail {
            average_heartrate: None,
            max_heartrate: None,
            total_elevation_gain: Some(240.0),
            average_cadence: None,
            splits_metric: vec![],
            laps: vec![],
            best_efforts: vec![],
        };
        let msg = build_user_message(
            "Zack",
            &act(99, "16", 12.4, Some(312)),
            Some(&detail),
            &[act(98, "14", 8.0, None)],
            &vol(),
        );
        assert!(msg.contains("elevation 240 m"));
        assert!(!msg.contains("avg HR"));
        assert!(!msg.contains("cadence"));
    }

    #[test]
    fn test_run_cadence_is_doubled_to_steps_per_minute() {
        // Strava reports foot-sport cadence per leg: 90 rpm == 180 spm.
        let detail = StravaActivityDetail {
            average_cadence: Some(89.5),
            ..StravaActivityDetail::default()
        };
        let msg = build_user_message(
            "Zack",
            &act(99, "16", 12.4, Some(312)),
            Some(&detail),
            &[act(98, "14", 8.0, None)],
            &vol(),
        );
        assert!(msg.contains("cadence 179 spm"), "msg was: {}", msg);
    }

    #[test]
    fn test_ride_cadence_is_not_doubled() {
        // Cycling cadence is genuine pedal RPM and must pass through as-is.
        let mut ride = act(99, "16", 40.0, None);
        ride.activity_type = ActivityType::Ride;
        let detail = StravaActivityDetail {
            average_cadence: Some(88.0),
            ..StravaActivityDetail::default()
        };
        let msg = build_user_message(
            "Zack",
            &ride,
            Some(&detail),
            &[act(98, "14", 8.0, None)],
            &vol(),
        );
        assert!(msg.contains("cadence 88 rpm"), "msg was: {}", msg);
    }

    #[test]
    fn test_history_has_column_headers_and_titles() {
        let mut prior = act(98, "14", 8.0, Some(327));
        prior.title = "Easy shakeout".into();
        let msg = build_user_message(
            "Zack",
            &act(99, "16", 12.4, Some(312)),
            None,
            &[prior],
            &vol(),
        );

        assert!(msg.contains("date"), "msg was: {}", msg);
        assert!(msg.contains("distance"), "msg was: {}", msg);
        assert!(msg.contains("title"), "msg was: {}", msg);
        assert!(msg.contains("Easy shakeout"), "msg was: {}", msg);
    }

    #[test]
    fn test_history_title_newlines_do_not_break_the_table() {
        let mut prior = act(98, "14", 8.0, Some(327));
        prior.title = "Morning\nrun\twith friends".into();
        let msg = build_user_message(
            "Zack",
            &act(99, "16", 12.4, Some(312)),
            None,
            &[prior],
            &vol(),
        );

        assert!(msg.contains("Morning run with friends"), "msg was: {}", msg);
        // One line per history row, plus the header — the title must not
        // introduce extra rows.
        let rows = msg.lines().filter(|l| l.contains("2026-08-14")).count();
        assert_eq!(rows, 1, "msg was: {}", msg);
    }

    #[test]
    fn test_splits_and_laps_and_pr_render() {
        let detail = StravaActivityDetail {
            average_heartrate: Some(152.0),
            max_heartrate: Some(171.0),
            total_elevation_gain: Some(240.0),
            average_cadence: Some(89.0),
            splits_metric: vec![StravaSplit {
                split: 1,
                distance: 1000.0,
                moving_time: 331,
                elapsed_time: 331,
                average_speed: Some(3.02),
                average_heartrate: Some(141.0),
                elevation_difference: Some(12.0),
            }],
            laps: vec![StravaLap {
                lap_index: 1,
                distance: 400.0,
                moving_time: 90,
                elapsed_time: 90,
                average_speed: Some(4.44),
                max_speed: Some(5.1),
                average_heartrate: Some(168.0),
                max_heartrate: Some(174.0),
                average_cadence: Some(93.0),
                total_elevation_gain: Some(2.0),
            }],
            best_efforts: vec![
                StravaBestEffort {
                    name: "5k".into(),
                    elapsed_time: 1264,
                    pr_rank: Some(2),
                },
                StravaBestEffort {
                    name: "1k".into(),
                    elapsed_time: 240,
                    pr_rank: None,
                },
            ],
        };
        let msg = build_user_message(
            "Zack",
            &act(99, "16", 12.4, Some(312)),
            Some(&detail),
            &[act(98, "14", 8.0, None)],
            &vol(),
        );

        assert!(msg.contains("SPLITS"));
        assert!(msg.contains("LAPS"));
        assert!(msg.contains("avg HR 152"));
        // Only PR-ranked efforts appear.
        assert!(msg.contains("5k in 21:04 — 2nd fastest ever"));
        assert!(!msg.contains("1k in"));
    }

    #[test]
    fn test_pr_phrase_ranks() {
        assert_eq!(pr_phrase(1), "fastest ever");
        assert_eq!(pr_phrase(2), "2nd fastest ever");
        assert_eq!(pr_phrase(3), "3rd fastest ever");
        assert_eq!(pr_phrase(4), "4th fastest ever");
    }

    #[test]
    fn test_pace_from_speed() {
        // 3.333 m/s == 300 s/km
        assert_eq!(pace_from_speed(Some(3.3333)), Some(300));
        assert_eq!(pace_from_speed(Some(0.0)), None);
        assert_eq!(pace_from_speed(None), None);
    }

    #[test]
    fn test_sanitize_passthrough_and_trim() {
        assert_eq!(
            sanitize("  Nice run.  ", 280),
            Some("Nice run.".to_string())
        );
    }

    #[test]
    fn test_sanitize_rejects_empty() {
        assert_eq!(sanitize("", 280), None);
        assert_eq!(sanitize("   \n  ", 280), None);
    }

    #[test]
    fn test_sanitize_truncates_at_word_boundary() {
        let out = sanitize("alpha bravo charlie delta", 16).unwrap();
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 16);
        // Cut between words, never mid-word.
        assert_eq!(out, "alpha bravo…");
    }

    #[test]
    fn test_sanitize_truncates_single_long_word() {
        let out = sanitize("supercalifragilistic", 10).unwrap();
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 10);
    }

    #[test]
    fn test_chunk_text_under_limit_is_single_chunk() {
        let chunks = chunk_text("one\ntwo\n", 100);
        assert_eq!(chunks, vec!["one\ntwo\n".to_string()]);
    }

    #[test]
    fn test_chunk_text_splits_on_line_boundaries() {
        // Each line is 6 chars including the newline; a limit of 12 fits two.
        let text = "aaaaa\nbbbbb\nccccc\nddddd\n";
        let chunks = chunk_text(text, 12);

        assert_eq!(chunks, vec!["aaaaa\nbbbbb\n", "ccccc\nddddd\n"]);
        // Nothing is lost or duplicated in the split.
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|c| c.chars().count() <= 12));
    }

    #[test]
    fn test_chunk_text_hard_splits_an_overlong_line() {
        // A single line longer than the limit must be split, not dropped.
        let text = format!("short\n{}\n", "x".repeat(25));
        let chunks = chunk_text(&text, 10);

        assert!(chunks.iter().all(|c| c.chars().count() <= 10));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn test_chunk_text_counts_by_char_not_byte() {
        // 20 CJK chars = 60 bytes. Chunking by byte would split mid-character
        // and panic; chunking by char must not.
        let text = "走".repeat(20);
        let chunks = chunk_text(&text, 8);

        assert!(chunks.iter().all(|c| c.chars().count() <= 8));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn test_chunk_text_zero_limit_is_empty() {
        assert!(chunk_text("anything", 0).is_empty());
    }
}
