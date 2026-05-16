use crate::types::ActivityType;
use chrono::Datelike;

#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn format_activity_message(
    athlete_name: &str,
    _activity_title: &str,
    activity_type: ActivityType,
    distance_km: f64,
    pace_sec_per_km: Option<i64>,
    duration_s: i64,
    week_km: f64,
    month_km: f64,
    activity_url: &str,
    incomplete_week: bool,
    incomplete_month: bool,
) -> String {
    let emoji = activity_type.emoji();
    let verb = activity_type.verb_past();

    let mut msg = format!("{} {} {} {:.1} km", emoji, athlete_name, verb, distance_km);

    // Duration line: either pace + duration, or just duration
    match pace_sec_per_km {
        Some(pace) => msg.push_str(&format!(
            "\n⏱ {} /km · {}",
            format_pace(pace),
            format_duration(duration_s),
        )),
        None => msg.push_str(&format!("\n⏱ {}", format_duration(duration_s),)),
    }

    // Mileage stats
    msg.push_str(&format!(
        "\n📏 Week: {:.1} km · Month: {:.1} km",
        week_km, month_km,
    ));

    // Incomplete warnings
    if incomplete_week {
        msg.push_str("\n⚠️ Week stats may be incomplete");
    }
    if incomplete_month {
        msg.push_str("\n⚠️ Month stats may be incomplete");
    }

    // Activity link
    msg.push_str(&format!("\n🔗 {}", activity_url));

    msg
}

#[must_use]
pub fn format_duration(total_seconds: i64) -> String {
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

#[must_use]
pub fn format_pace(sec_per_km: i64) -> String {
    let minutes = sec_per_km / 60;
    let seconds = sec_per_km % 60;
    format!("{}:{:02}", minutes, seconds)
}

/// Returns (`incomplete_week`, `incomplete_month`)
#[must_use]
pub fn incomplete_periods(oldest_date: Option<chrono::NaiveDate>) -> (bool, bool) {
    let Some(oldest) = oldest_date else {
        return (true, true);
    };

    let today = chrono::Local::now().date_naive();

    // Monday of current week
    let monday = today - chrono::Duration::days(i64::from(today.weekday().num_days_from_monday()));

    // First of current month
    let first_of_month =
        chrono::NaiveDate::from_ymd_opt(today.year(), today.month(), 1).expect("valid date");

    (oldest > monday, oldest > first_of_month)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(3661), "1:01:01");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(3723), "1:02:03");
    }

    #[test]
    fn test_format_duration_under_hour() {
        assert_eq!(format_duration(270), "4:30");
    }

    #[test]
    fn test_format_pace() {
        assert_eq!(format_pace(286), "4:46");
    }

    #[test]
    fn test_format_pace_rounds() {
        assert_eq!(format_pace(270), "4:30");
    }

    #[test]
    fn test_activity_emoji() {
        assert_eq!(ActivityType::Run.emoji(), "🏃");
        assert_eq!(ActivityType::TrailRun.emoji(), "🏃");
        assert_eq!(ActivityType::VirtualRun.emoji(), "🏃");
        assert_eq!(ActivityType::Hike.emoji(), "🥾");
        assert_eq!(ActivityType::Walk.emoji(), "🚶");
        assert_eq!(ActivityType::Ride.emoji(), "🚴");
        assert_eq!(ActivityType::Swim.emoji(), "🏅");
    }

    #[test]
    fn test_incomplete_periods_no_data() {
        let (week, month) = incomplete_periods(None);
        assert!(week);
        assert!(month);
    }

    #[test]
    fn test_incomplete_periods_old_data() {
        let old = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let (week, month) = incomplete_periods(Some(old));
        assert!(!week);
        assert!(!month);
    }

    #[test]
    fn test_format_activity_message_basic() {
        let msg = format_activity_message(
            "zack",
            "Afternoon Run",
            ActivityType::Run,
            10.2,
            Some(286), // 4:46 /km
            2982,      // 49:42
            34.5,      // week km
            128.3,     // month km
            "https://strava.com/activities/123",
            false,
            false,
        );

        assert!(msg.contains("🏃 zack ran 10.2 km"));
        assert!(msg.contains("4:46 /km"));
        assert!(msg.contains("49:42"));
        assert!(msg.contains("Week: 34.5 km"));
        assert!(msg.contains("Month: 128.3 km"));
        assert!(msg.contains("https://strava.com/activities/123"));
    }

    #[test]
    fn test_format_activity_message_hike() {
        let msg = format_activity_message(
            "bob",
            "Hill Climb",
            ActivityType::Hike,
            5.0,
            None,
            3600,
            10.0,
            25.0,
            "https://strava.com/activities/456",
            false,
            false,
        );

        assert!(msg.contains("🥾 bob hiked 5.0 km"));
        assert!(msg.contains("1:00:00"));
        assert!(!msg.contains("/km"));
    }

    #[test]
    fn test_format_with_incomplete_warning() {
        let msg = format_activity_message(
            "alice",
            "Run",
            ActivityType::Run,
            5.0,
            Some(300),
            1500,
            5.0,
            5.0,
            "https://example.com/1",
            true,
            false,
        );
        assert!(msg.contains("⚠️ Week stats may be incomplete"));
    }
}
