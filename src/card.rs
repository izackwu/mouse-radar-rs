use crate::types::ActivityType;
use anyhow::Result;

const SVG_TEMPLATE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="450" viewBox="0 0 640 450" text-rendering="optimizeLegibility">
  <rect width="640" height="450" fill="#ffffff" rx="0"/>
  <line x1="0" y1="120" x2="640" y2="120" stroke="#fc4c02" stroke-width="3"/>
  <text x="36" y="92" font-size="56" font-family="Apple Color Emoji">{{EMOJI}}</text>
  <text x="96" y="64" font-size="20" fill="#999999" font-weight="500" font-family="Inter">{{NAME}} just finished a {{ACTIVITY_NOUN}}</text>
  <text x="96" y="96" font-size="28" fill="#1a1a1a" font-family="Inter" font-weight="700">{{TITLE}}</text>
  <text x="604" y="64" font-size="16" fill="#999999" font-family="Inter" text-anchor="end">{{DATE}} · {{TIME}}</text>
  <text x="320" y="228" font-size="88" fill="#fc4c02" font-family="Inter" font-weight="800" text-anchor="middle">{{DISTANCE}}<tspan font-size="32" fill="#999999" font-family="Inter" font-weight="500"> km</tspan></text>
  <text x="320" y="270" font-size="22" fill="#555555" font-family="Inter" text-anchor="middle" {{PACE_STYLE}}>
    <tspan font-family="Apple Color Emoji">🏁</tspan> {{PACE}}/km  ·  <tspan font-family="Apple Color Emoji">⏱</tspan> {{DURATION}}
  </text>
  <line x1="36" y1="300" x2="604" y2="300" stroke="#f0f0f0" stroke-width="2"/>
  <text x="170" y="360" font-size="16" fill="#aaaaaa" font-family="Inter" font-weight="400" text-anchor="middle">THIS WEEK</text>
  <text x="170" y="410" font-size="34" fill="#1a1a1a" font-family="Inter" font-weight="700" text-anchor="middle">{{WEEK}}<tspan font-size="20" fill="#aaaaaa" font-family="Inter"> km</tspan><tspan font-size="20" fill="#fc4c02" font-family="Inter">{{WEEK_ASTERISK}}</tspan></text>
  <line x1="338" y1="316" x2="338" y2="440" stroke="#f0f0f0" stroke-width="1.5"/>
  <text x="472" y="360" font-size="16" fill="#aaaaaa" font-family="Inter" font-weight="400" text-anchor="middle">THIS MONTH</text>
  <text x="472" y="410" font-size="34" fill="#1a1a1a" font-family="Inter" font-weight="700" text-anchor="middle">{{MONTH}}<tspan font-size="20" fill="#aaaaaa" font-family="Inter"> km</tspan><tspan font-size="20" fill="#fc4c02" font-family="Inter">{{MONTH_ASTERISK}}</tspan></text>
</svg>"##;

/// Truncate a string to `max_chars` characters, appending "..." if truncated.
/// Handles multi-byte unicode correctly by operating on `char`s.
#[must_use]
pub fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > max_chars {
        let truncated: String = chars[..max_chars].iter().collect();
        format!("{truncated}...")
    } else {
        s.to_string()
    }
}

/// Replace `{{TOKEN}}` placeholders in the template with values from `tokens`.
#[must_use]
pub fn fill_template(template: &str, tokens: &[(&str, &str)]) -> String {
    let mut result = template.to_string();
    for (key, value) in tokens {
        let placeholder = format!("{{{{{key}}}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

/// Data needed to render an activity card image.
pub struct CardData {
    pub activity_type: ActivityType,
    pub athlete_name: String,
    pub title: String,
    pub date: String,
    pub time_str: String,
    pub distance_km: f64,
    pub pace_sec_per_km: Option<i64>,
    pub duration_s: i64,
    pub week_km: f64,
    pub month_km: f64,
    pub incomplete_week: bool,
    pub incomplete_month: bool,
}

/// Format the caption that accompanies the card photo.
#[must_use]
pub fn format_caption(url: &str, incomplete_week: bool, incomplete_month: bool) -> String {
    let mut caption = url.to_string();
    if incomplete_week {
        caption.push_str("\n* Week stats may be incomplete");
    }
    if incomplete_month {
        caption.push_str("\n* Month stats may be incomplete");
    }
    caption
}

/// Render an activity card SVG, returning PNG bytes at the given scale.
///
/// `scale` multiplies the 640x450 logical SVG dimensions (e.g., 4.0 -> 2560x1800 px).
/// The Inter fonts are compiled into the binary; emoji rendering relies on system fonts
/// (Apple Color Emoji on macOS, Noto Color Emoji on Linux).
///
/// # Errors
///
/// Returns an error if the SVG cannot be parsed, fonts cannot be loaded, or rendering fails.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn render_card(data: &CardData, scale: f32) -> Result<Vec<u8>> {
    let distance = format!("{:.1}", data.distance_km);
    let duration = format_duration(data.duration_s);
    let week = format_km(data.week_km);
    let month_km = format_km(data.month_km);

    let pace_style = if data.activity_type.has_pace() {
        ""
    } else {
        "visibility=\"hidden\""
    };

    let pace = if let Some(sec) = data.pace_sec_per_km {
        format_pace(sec)
    } else {
        String::new()
    };

    let week_asterisk = if data.incomplete_week { "*" } else { "" };
    let month_asterisk = if data.incomplete_month { "*" } else { "" };

    let svg = fill_template(
        SVG_TEMPLATE,
        &[
            ("EMOJI", data.activity_type.emoji()),
            ("NAME", &truncate(&data.athlete_name, 20)),
            ("ACTIVITY_NOUN", data.activity_type.noun()),
            ("TITLE", &truncate(&data.title, 40)),
            ("DATE", &data.date),
            ("TIME", &data.time_str),
            ("DISTANCE", &distance),
            ("PACE", &pace),
            ("DURATION", &duration),
            ("WEEK", &week),
            ("WEEK_ASTERISK", week_asterisk),
            ("MONTH", &month_km),
            ("MONTH_ASTERISK", month_asterisk),
            ("PACE_STYLE", pace_style),
        ],
    );

    let inter_regular = include_bytes!("../fonts/Inter-Regular.ttf");
    let inter_bold = include_bytes!("../fonts/Inter-Bold.ttf");

    let mut fontdb = usvg::fontdb::Database::new();
    fontdb.load_font_data(inter_regular.to_vec());
    fontdb.load_font_data(inter_bold.to_vec());

    let opts = usvg::Options::default();
    let tree = usvg::Tree::from_str(&svg, &opts)?;

    let width = (640.0_f32 * scale) as u32;
    let height = (450.0_f32 * scale) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| anyhow::anyhow!("Failed to create pixmap"))?;

    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    pixmap.encode_png().map_err(Into::into)
}

fn format_duration(total_seconds: i64) -> String {
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn format_pace(sec_per_km: i64) -> String {
    let minutes = sec_per_km / 60;
    let seconds = sec_per_km % 60;
    format!("{minutes}:{seconds:02}")
}

fn format_km(km: f64) -> String {
    let formatted = format!("{km:.1}");
    if formatted.ends_with(".0") {
        formatted[..formatted.len() - 2].to_string()
    } else {
        formatted
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;
    use std::fs;

    /// Helper to build a reasonable default `CardData` for testing.
    fn default_card() -> CardData {
        CardData {
            activity_type: ActivityType::Run,
            athlete_name: "Alice".into(),
            title: "Morning Run".into(),
            date: "May 16".into(),
            time_str: "8:30 AM".into(),
            distance_km: 10.2,
            pace_sec_per_km: Some(286),
            duration_s: 2982,
            week_km: 34.5,
            month_km: 128.3,
            incomplete_week: false,
            incomplete_month: false,
        }
    }

    #[test]
    fn test_truncate_ascii() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    #[test]
    fn test_truncate_emoji() {
        assert_eq!(truncate("🏃🥾", 1), "🏃...");
        assert_eq!(truncate("a🏃b🥾c", 3), "a🏃b...");
    }

    #[test]
    fn test_truncate_noop() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_format_caption_normal() {
        let cap = format_caption("https://strava.com/activities/1", false, false);
        assert_eq!(cap, "https://strava.com/activities/1");
    }

    #[test]
    fn test_format_caption_incomplete() {
        let cap = format_caption("https://strava.com/activities/1", true, true);
        assert!(cap.contains("* Week stats may be incomplete"));
        assert!(cap.contains("* Month stats may be incomplete"));
    }

    /// Renders a card, verifies PNG output is non-empty and starts with PNG magic bytes.
    fn check_png(data: &CardData, scale: f32) -> Vec<u8> {
        let png = render_card(data, scale).expect("render_card should succeed");
        // Check non-empty
        assert!(!png.is_empty(), "PNG output should not be empty");
        // Check PNG magic: 89 50 4E 47 0D 0A 1A 0A
        assert_eq!(png[..4], [137, 80, 78, 71], "should start with PNG header");
        // Check IHDR at byte 12-15
        assert_eq!(png[12..16], [73, 72, 68, 82], "should contain IHDR chunk");
        png
    }

    #[test]
    fn test_render_card_basic_run() {
        let data = default_card();
        check_png(&data, 4.0);
    }

    #[test]
    fn test_render_card_no_pace() {
        let data = CardData {
            activity_type: ActivityType::Hike,
            pace_sec_per_km: None,
            ..default_card()
        };
        check_png(&data, 4.0);
    }

    #[test]
    fn test_render_card_partial_data() {
        let data = CardData {
            incomplete_week: true,
            incomplete_month: true,
            ..default_card()
        };
        check_png(&data, 4.0);
    }

    #[test]
    fn test_render_card_long_title() {
        let data = CardData {
            title: "A very long run title that should be truncated in the card".into(),
            ..default_card()
        };
        check_png(&data, 4.0);
    }

    #[test]
    fn test_render_card_long_name() {
        let data = CardData {
            athlete_name: "Alexander Benjamin Christopher".into(),
            ..default_card()
        };
        check_png(&data, 4.0);
    }

    #[test]
    fn test_render_card_4x_dimensions() {
        let data = default_card();
        let png = check_png(&data, 4.0);
        // For a 4x render at 640x450 base, output should be 2560x1800
        // Read width/height from PNG IHDR (bytes 16-19 = width, 20-23 = height, big-endian)
        let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        assert_eq!(width, 2560, "4x width should be 2560");
        assert_eq!(height, 1800, "4x height should be 1800");
    }

    #[test]
    fn test_render_card_unknown_type() {
        let data = CardData {
            activity_type: ActivityType::Other,
            pace_sec_per_km: None,
            ..default_card()
        };
        check_png(&data, 4.0);
    }

    #[test]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn test_render_card_different_scales() {
        // Verify rendering works at various scales
        let data = default_card();
        for scale in [1.0, 2.0, 3.0] {
            let png = render_card(&data, scale).expect("should render at any scale");
            let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
            let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
            assert_eq!(width, (640.0_f32 * scale) as u32);
            assert_eq!(height, (450.0_f32 * scale) as u32);
        }
    }

    #[test]
    fn test_render_card_swim_no_pace() {
        // Swim has no pace
        let data = CardData {
            activity_type: ActivityType::Swim,
            pace_sec_per_km: None,
            ..default_card()
        };
        check_png(&data, 4.0);
    }

    #[test]
    fn test_render_card_ride_with_pace() {
        // Ride has pace (speed in min/km)
        let data = CardData {
            activity_type: ActivityType::Ride,
            pace_sec_per_km: Some(120),
            distance_km: 40.0,
            duration_s: 7200,
            ..default_card()
        };
        check_png(&data, 4.0);
    }

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
    fn test_format_pace_standard() {
        assert_eq!(format_pace(286), "4:46");
    }

    #[test]
    fn test_format_pace_round() {
        assert_eq!(format_pace(270), "4:30");
    }

    #[test]
    fn test_format_km_whole() {
        assert_eq!(format_km(34.0), "34");
    }

    #[test]
    fn test_format_km_fractional() {
        assert_eq!(format_km(34.5), "34.5");
    }

    #[test]
    fn test_format_km_small() {
        assert_eq!(format_km(5.1), "5.1");
    }

    #[test]
    fn test_fill_template_replaces_tokens() {
        let result = fill_template("Hello {{NAME}}!", &[("NAME", "World")]);
        assert_eq!(result, "Hello World!");
    }

    #[test]
    fn test_fill_template_multiple_tokens() {
        let result = fill_template("{{A}}-{{B}}", &[("A", "1"), ("B", "2")]);
        assert_eq!(result, "1-2");
    }

    /// Generate snapshot PNGs for visual inspection, written to `card-snapshots/`.
    #[test]
    fn generate_snapshots() {
        let dir = "card-snapshots";
        let _ = fs::create_dir_all(dir);

        // Run — all stats present
        let data = default_card();
        let png = render_card(&data, 4.0).expect("render default card");
        fs::write(format!("{dir}/01-run.png"), &png).unwrap();

        // Hike — no pace, different emoji
        let data = CardData {
            activity_type: ActivityType::Hike,
            athlete_name: "Bob".into(),
            title: "Mountain Trail".into(),
            distance_km: 8.5,
            pace_sec_per_km: None,
            duration_s: 5400,
            ..default_card()
        };
        let png = render_card(&data, 4.0).expect("render hike");
        fs::write(format!("{dir}/02-hike.png"), &png).unwrap();

        // Ride — different emoji, long distance
        let data = CardData {
            activity_type: ActivityType::Ride,
            title: "Century Ride".into(),
            distance_km: 161.0,
            pace_sec_per_km: Some(180),
            duration_s: 28_800,
            week_km: 200.0,
            month_km: 850.0,
            ..default_card()
        };
        let png = render_card(&data, 4.0).expect("render ride");
        fs::write(format!("{dir}/03-ride.png"), &png).unwrap();

        // Incomplete stats
        let data = CardData {
            title: "Evening Run".into(),
            distance_km: 6.2,
            incomplete_week: true,
            incomplete_month: true,
            ..default_card()
        };
        let png = render_card(&data, 4.0).expect("render incomplete");
        fs::write(format!("{dir}/04-incomplete.png"), &png).unwrap();

        // Long title + name
        let data = CardData {
            athlete_name: "Alexandria The Great Runner".into(),
            title: "A very long run title for testing truncation behavior".into(),
            ..default_card()
        };
        let png = render_card(&data, 4.0).expect("render long text");
        fs::write(format!("{dir}/05-long-text.png"), &png).unwrap();

        // Swim (Other-like, no pace)
        let data = CardData {
            activity_type: ActivityType::Swim,
            athlete_name: "Michael".into(),
            title: "Pool Session".into(),
            distance_km: 2.0,
            pace_sec_per_km: None,
            duration_s: 3600,
            ..default_card()
        };
        let png = render_card(&data, 4.0).expect("render swim");
        fs::write(format!("{dir}/06-swim.png"), &png).unwrap();

        // File size sanity
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            assert!(
                meta.len() > 1000,
                "snapshot {} should be > 1KB",
                entry.file_name().to_string_lossy()
            );
        }
    }
}
