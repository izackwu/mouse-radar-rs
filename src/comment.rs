use crate::db::CachedActivity;
use crate::formatting::{format_duration, format_pace};
use crate::strava::StravaActivityDetail;

/// Persona and guardrails for the comment. Overridable via `AI_SYSTEM_PROMPT`.
///
/// The qualitative-comparison rule is deliberate: the model is handed a raw
/// history table and does its own arithmetic, so the prompt biases it toward
/// claims that stay true under approximation.
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
- Observe what happened. Do not prescribe future workouts.
- If the history is thin, keep it general — never invent a trend.
- Plain text only: no markdown, no emoji, no greeting or sign-off.";

/// History length at or below which the model is told not to infer trends.
const THIN_HISTORY_THRESHOLD: usize = 2;

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

/// Build the user half of the prompt: the activity, its detail, and the
/// athlete's recent history.
#[must_use]
pub fn build_user_message(
    athlete_name: &str,
    activity: &CachedActivity,
    detail: Option<&StravaActivityDetail>,
    history: &[CachedActivity],
) -> String {
    let mut s = String::new();

    s.push_str(&format!("ATHLETE: {}\n\n", athlete_name));
    s.push_str("THIS ACTIVITY\n");
    s.push_str(&format!(
        "  {} \"{}\" — {}\n",
        activity.activity_type, activity.title, activity.start_date_local
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
            extras.push(format!("cadence {:.0}", c));
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
            s.push_str("\n  LAPS\n");
            s.push_str("    #    dist      pace   avgHR  maxHR  cad   elev\n");
            for lap in &d.laps {
                s.push_str(&format!(
                    "    {:<4} {:>5.2} km  {:>5}  {:>5}  {:>5}  {:>4}  {:>5}\n",
                    lap.lap_index,
                    lap.distance / 1000.0,
                    opt_pace(lap.average_speed),
                    opt_num(lap.average_heartrate),
                    opt_num(lap.max_heartrate),
                    opt_num(lap.average_cadence),
                    opt_signed(lap.total_elevation_gain),
                ));
            }
        }
    }

    s.push_str(&format!(
        "\nRECENT HISTORY (newest first — {} activities)\n",
        history.len()
    ));
    for h in history {
        let date = h.start_date_local.get(..10).unwrap_or(&h.start_date_local);
        let pace = h
            .pace_sec_per_km
            .map_or_else(|| "—".to_string(), format_pace);
        // Bind to a String first: column padding (`{:<6}`) is only honoured by
        // Display impls that route through `f.pad`, which strum's derive does
        // not guarantee. `String`'s impl does.
        let kind = h.activity_type.to_string();
        s.push_str(&format!(
            "  {}  {:<6} {:>6.1} km  {:>7}  {}\n",
            date,
            kind,
            h.distance_km,
            pace,
            format_duration(h.duration_s),
        ));
    }

    if history.len() <= THIN_HISTORY_THRESHOLD {
        let (count, verb) = if history.len() == 1 {
            ("1 prior activity", "is")
        } else {
            ("2 prior activities", "are")
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

    #[test]
    fn test_user_message_includes_activity_and_history() {
        let current = act(99, "16", 12.4, Some(312));
        let history = vec![
            act(98, "14", 8.0, Some(327)),
            act(97, "12", 10.2, Some(331)),
        ];
        let msg = build_user_message("Zack", &current, None, &history);

        assert!(msg.contains("ATHLETE: Zack"));
        assert!(msg.contains("12.4 km"));
        assert!(msg.contains("5:12/km"));
        assert!(msg.contains("RECENT HISTORY"));
        assert!(msg.contains("2026-08-14"));
        assert!(msg.contains("8.0 km"));
    }

    #[test]
    fn test_thin_history_hint_fires_at_two_or_fewer() {
        let current = act(99, "16", 12.4, Some(312));
        let thin = vec![act(98, "14", 8.0, Some(327))];
        assert!(
            build_user_message("Zack", &current, None, &thin).contains("do not describe trends")
        );

        let thick: Vec<CachedActivity> =
            (1..=5).map(|i| act(90 + i, "10", 8.0, Some(320))).collect();
        assert!(
            !build_user_message("Zack", &current, None, &thick).contains("do not describe trends")
        );
    }

    #[test]
    fn test_no_detail_omits_all_detail_blocks() {
        let msg = build_user_message(
            "Zack",
            &act(99, "16", 12.4, Some(312)),
            None,
            &[act(98, "14", 8.0, None)],
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
        );
        assert!(msg.contains("elevation 240 m"));
        assert!(!msg.contains("avg HR"));
        assert!(!msg.contains("cadence"));
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
}
