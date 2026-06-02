use crate::types::ActivityType;
use anyhow::Result;
use std::sync::LazyLock;

static FONTS: LazyLock<std::sync::Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_font_data(include_bytes!("../fonts/Inter-Regular.ttf").to_vec());
    db.load_font_data(include_bytes!("../fonts/Inter-Bold.ttf").to_vec());
    db.load_font_data(include_bytes!("../fonts/emoji-subset.ttf").to_vec());

    // Preload the platform's color-emoji font BEFORE load_system_fonts, so it ranks first
    // in usvg's fallback iteration. Otherwise codepoints like ☕ U+2615 (text-default emoji)
    // get picked up by monochrome fonts (Menlo, Apple Symbols, STIX) which iterate earlier.
    #[cfg(target_os = "macos")]
    let _ = db.load_font_file("/System/Library/Fonts/Apple Color Emoji.ttc");
    #[cfg(target_os = "linux")]
    let _ = db.load_font_file("/usr/share/fonts/noto/NotoColorEmoji.ttf");

    // CJK + full-emoji fallback. macOS dev: Apple Color Emoji, PingFang, Apple SD Gothic Neo,
    // etc. Linux/Docker: Noto CJK + Noto Color Emoji installed via apk in the Dockerfile.
    db.load_system_fonts();

    // LastResort.otf (macOS) claims to support every Unicode codepoint by design (it's
    // the OS's "no glyph" hint font). When present, usvg's fallback resolver picks it
    // first and renders everything as "?" boxes. macOS registers it under both
    // "LastResort" and ".LastResort" (the dot-prefixed internal name), so match both.
    let last_resort_ids: Vec<_> = db
        .faces()
        .filter(|f| {
            f.families
                .iter()
                .any(|(name, _)| name.trim_start_matches('.').starts_with("LastResort"))
        })
        .map(|f| f.id)
        .collect();
    for id in last_resort_ids {
        db.remove_face(id);
    }

    std::sync::Arc::new(db)
});

const SVG_TEMPLATE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="450" viewBox="0 0 640 450" text-rendering="optimizeLegibility">
  <rect width="640" height="450" fill="#ffffff" rx="0"/>
  <line x1="0" y1="120" x2="640" y2="120" stroke="#fc4c02" stroke-width="3"/>
  <text x="36" y="92" font-size="56" font-family="Apple Color Emoji">{{EMOJI}}</text>
  <text x="96" y="64" font-size="20" fill="#999999" font-weight="500" font-family="Inter">{{NAME}} just finished a {{ACTIVITY_NOUN}}</text>
  <text x="96" y="96" font-size="28" fill="#1a1a1a" font-family="Inter" font-weight="700">{{TITLE}}</text>
  <text x="604" y="64" font-size="16" fill="#999999" font-family="Inter" text-anchor="end">{{DATE}} · {{TIME}}</text>
  <text x="320" y="228" font-size="88" fill="#fc4c02" font-family="Inter" font-weight="800" text-anchor="middle">{{DISTANCE}}<tspan font-size="32" fill="#999999" font-family="Inter" font-weight="500"> km</tspan></text>
  {{PACE_ROW}}
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

/// Strip the two emoji presentation selectors (VS-15 U+FE0E, VS-16 U+FE0F) from text.
///
/// **What these selectors do:** Unicode reserves U+FE00..U+FE0F as variation selectors.
/// VS-15 and VS-16 are the emoji-related pair:
///   - **VS-16 (U+FE0F)** — "emoji presentation". Appended after a text-default codepoint
///     like ☁ U+2601 to opt into the color emoji glyph. Most user-facing ☁️ ✈️ ⭐️ ⚠️ ❄️
///     input is actually two codepoints: base + FE0F.
///   - **VS-15 (U+FE0E)** — "text presentation". The opposite hint: render as the
///     monochrome text glyph instead of the color emoji.
///
/// **Why we strip them:** usvg's font fallback fails to render a base+VS sequence even
/// when the chosen face (e.g. Apple Color Emoji) has *both* codepoints in its cmap —
/// the whole run drops to the renderer's "no glyph" rectangle. Stripping the selector
/// lets the base codepoint flow through normal fallback, and our preloaded color-emoji
/// font already biases the result toward the color variant, which is what users want
/// in a Strava activity card.
///
/// **Why we don't strip the broader VS range:** the supplementary block U+E0100..U+E01EF
/// (VS-17..VS-256) encodes Ideographic Variation Sequences for CJK — most notably the
/// specific historical glyph for a Japanese family name's kanji (e.g. the variant of 邊
/// in 渡邊). Stripping those would silently render the wrong glyph for that person. We
/// haven't seen usvg break on those, so we leave them alone. VS-1..VS-14 and the
/// Mongolian FVS block (U+180B..U+180D, U+180F) are similarly rare in our input domain
/// and not stripped.
#[must_use]
pub fn strip_emoji_presentation_selectors(s: &str) -> String {
    s.chars()
        .filter(|&c| c != '\u{FE0E}' && c != '\u{FE0F}')
        .collect()
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
    pub start_date_local: String, // ISO 8601 datetime in athlete's local timezone
    pub distance_km: f64,
    pub pace_sec_per_km: Option<i64>,
    pub duration_s: i64,
    pub week_km: f64,
    pub month_km: f64,
    pub incomplete_week: bool,
    pub incomplete_month: bool,
}

/// The result of building a notification: text message, card image, and caption.
pub struct Notification {
    pub text: String,
    pub card_png: Vec<u8>,
    pub caption: String,
}

/// Format the caption that accompanies the card photo. Output is Telegram
/// `MarkdownV2`; callers must send with `ParseMode::MarkdownV2`.
#[must_use]
pub fn format_caption(athlete_name: &str, url: &str) -> String {
    debug_assert!(
        !url.contains(')') && !url.contains('\\'),
        "URL contains characters that would break MarkdownV2 link syntax: {url}"
    );
    format!("{athlete_name} just finished a workout\\! Check it out on [Strava]({url})")
}

/// Render an activity card SVG, returning PNG bytes at the given scale.
///
/// `scale` multiplies the 640×450 logical SVG dimensions (e.g., 4 → 2560×1800 px).
/// Inter and emoji fonts are compiled into the binary via `include_bytes!`.
///
/// # Errors
///
/// Returns an error if the SVG cannot be parsed or rendering fails.
#[allow(clippy::cast_precision_loss)]
pub fn render_card(data: &CardData, scale: u32) -> Result<Vec<u8>> {
    let (date_str, time_str) = parse_local_datetime(&data.start_date_local);
    let distance = format!("{:.1}", data.distance_km);
    let duration = crate::formatting::format_duration(data.duration_s);
    let week = format!("{:.1}", data.week_km);
    let month_km = format!("{:.1}", data.month_km);

    let pace_row = if let Some(sec) = data.pace_sec_per_km {
        let pace_str = crate::formatting::format_pace(sec);
        format!(
            r##"  <text x="320" y="270" font-size="22" fill="#555555" font-family="Inter" text-anchor="middle">
    <tspan font-family="Apple Color Emoji">🏁</tspan> {pace_str}/km  ·  <tspan font-family="Apple Color Emoji">⏱</tspan> {duration}
  </text>"##
        )
    } else {
        String::new()
    };

    let week_asterisk = if data.incomplete_week { "*" } else { "" };
    let month_asterisk = if data.incomplete_month { "*" } else { "" };

    let svg = fill_template(
        SVG_TEMPLATE,
        &[
            ("EMOJI", data.activity_type.emoji()),
            (
                "NAME",
                &truncate(&strip_emoji_presentation_selectors(&data.athlete_name), 12),
            ),
            ("ACTIVITY_NOUN", data.activity_type.noun()),
            (
                "TITLE",
                &truncate(&strip_emoji_presentation_selectors(&data.title), 40),
            ),
            ("DATE", &date_str),
            ("TIME", &time_str),
            ("DISTANCE", &distance),
            ("PACE_ROW", &pace_row),
            ("WEEK", &week),
            ("WEEK_ASTERISK", week_asterisk),
            ("MONTH", &month_km),
            ("MONTH_ASTERISK", month_asterisk),
        ],
    );

    let opts = usvg::Options {
        fontdb: std::sync::Arc::clone(&FONTS),
        ..Default::default()
    };

    let tree = usvg::Tree::from_str(&svg, &opts)?;

    let width = 640 * scale;
    let height = 450 * scale;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| anyhow::anyhow!("Failed to create pixmap"))?;

    let transform = tiny_skia::Transform::from_scale(scale as f32, scale as f32);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    pixmap.encode_png().map_err(Into::into)
}

/// Parse an ISO 8601 datetime string into ("Mon DD", "H:MM AM") format.
/// Returns empty strings if parsing fails.
fn parse_local_datetime(iso: &str) -> (String, String) {
    let dt = chrono::DateTime::parse_from_rfc3339(iso).or_else(|_| {
        chrono::NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%SZ").map(|d| d.and_utc().into())
    });
    match dt {
        Ok(dt) => {
            let date = dt.format("%b %e").to_string();
            let time = dt.format("%-I:%M %p").to_string();
            (date.trim().to_string(), time)
        }
        Err(_) => (String::new(), String::new()),
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
            start_date_local: "2026-05-16T08:30:00Z".into(),
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
    fn test_format_caption() {
        let cap = format_caption("Randy", "https://strava.com/activities/1");
        assert_eq!(
            cap,
            "Randy just finished a workout! Check it out on [Strava](https://strava.com/activities/1)"
        );
    }

    /// Renders a card, verifies PNG output is non-empty and starts with PNG magic bytes.
    fn check_png(data: &CardData, scale: u32) -> Vec<u8> {
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
        check_png(&data, 4);
    }

    #[test]
    fn test_render_card_no_pace() {
        let data = CardData {
            activity_type: ActivityType::Hike,
            pace_sec_per_km: None,
            ..default_card()
        };
        check_png(&data, 4);
    }

    #[test]
    fn test_render_card_partial_data() {
        let data = CardData {
            incomplete_week: true,
            incomplete_month: true,
            ..default_card()
        };
        check_png(&data, 4);
    }

    #[test]
    fn test_render_card_long_title() {
        let data = CardData {
            title: "A very long run title that should be truncated in the card".into(),
            ..default_card()
        };
        check_png(&data, 4);
    }

    #[test]
    fn test_render_card_long_name() {
        let data = CardData {
            athlete_name: "Alexander Benjamin Christopher".into(),
            ..default_card()
        };
        check_png(&data, 4);
    }

    #[test]
    fn test_render_card_4x_dimensions() {
        let data = default_card();
        let png = check_png(&data, 4);
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
        check_png(&data, 4);
    }

    #[test]
    fn test_render_card_different_scales() {
        let data = default_card();
        for &scale in &[1, 2, 3] {
            let png = render_card(&data, scale).expect("should render at any scale");
            let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
            let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
            assert_eq!(width, 640 * scale);
            assert_eq!(height, 450 * scale);
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
        check_png(&data, 4);
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
        check_png(&data, 4);
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
        let png = render_card(&data, 4).expect("render default card");
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
        let png = render_card(&data, 4).expect("render hike");
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
        let png = render_card(&data, 4).expect("render ride");
        fs::write(format!("{dir}/03-ride.png"), &png).unwrap();

        // Incomplete stats
        let data = CardData {
            title: "Evening Run".into(),
            distance_km: 6.2,
            incomplete_week: true,
            incomplete_month: true,
            ..default_card()
        };
        let png = render_card(&data, 4).expect("render incomplete");
        fs::write(format!("{dir}/04-incomplete.png"), &png).unwrap();

        // Long title + name
        let data = CardData {
            athlete_name: "Alexandria The Great Runner".into(),
            title: "A very long run title for testing truncation behavior".into(),
            ..default_card()
        };
        let png = render_card(&data, 4).expect("render long text");
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
        let png = render_card(&data, 4).expect("render swim");
        fs::write(format!("{dir}/06-swim.png"), &png).unwrap();

        // Emoji in title and name — renders via system font fallback
        let data = CardData {
            activity_type: ActivityType::Run,
            athlete_name: "👑 Queen Alice".into(),
            title: "☕ Morning Coffee Run with friends 🥐🥳".into(),
            ..default_card()
        };
        let png = render_card(&data, 4).expect("render emoji title");
        fs::write(format!("{dir}/07-emoji-title.png"), &png).unwrap();

        // CJK characters — should render correctly
        let data = CardData {
            activity_type: ActivityType::Run,
            athlete_name: "中文用户".into(),
            title: "日本語ランニング".into(),
            ..default_card()
        };
        let png = render_card(&data, 4).expect("render cjk");
        fs::write(format!("{dir}/08-cjk.png"), &png).unwrap();

        // Mixed CJK + emoji name + Korean title with emojis.
        //
        // Single-codepoint emojis only here. usvg's font fallback in mixed-script text has
        // known limitations (linebender/resvg#861, linebender/resvg#916):
        //   - ZWJ-joined emoji (e.g. 🏃‍♀️ = 🏃 + ZWJ + ♀ + VS-16) never compose; at best
        //     the base char renders and the rest is dropped.
        //   - Regional-indicator flag pairs (e.g. 🇰🇷) render bare but disappear once mixed
        //     with any other text — even when wrapped in their own <tspan>.
        //   - Linux/Noto only: CJK chars followed by a non-ASCII-presentation emoji in the
        //     same text run render as tofu (e.g. "黑影儿📺" → 4 boxes). Apple Color Emoji
        //     on macOS doesn't trip this. Title `🏃 서울 마라톤 🏆` is fine on both because
        //     the emojis are separated from the Korean by spaces.
        // Wrapping each script run in its own <tspan> works for Korean + simple emoji, but
        // doesn't recover flags or ZWJ sequences, so we just avoid them in the test fixture.
        let data = CardData {
            activity_type: ActivityType::Run,
            athlete_name: "黑影儿📺".into(),
            title: "🏃 서울 마라톤 🏆".into(),
            ..default_card()
        };
        let png = render_card(&data, 4).expect("render cjk+emoji");
        fs::write(format!("{dir}/09-cjk-emoji.png"), &png).unwrap();

        // Emoji presentation selectors VS-15 (U+FE0E) and VS-16 (U+FE0F).
        //
        // Text-default symbols like ☁ U+2601, ✈ U+2708, ⌚ U+231A take a trailing
        // FE0F to render as color emoji. usvg's fallback drops the whole run to
        // "no glyph" when it sees the base+VS sequence — see
        // strip_emoji_presentation_selectors. This fixture sends both VS-15 and
        // VS-16 through render_card to lock in that they get stripped and the
        // base glyphs render normally.
        let data = CardData {
            activity_type: ActivityType::Run,
            athlete_name: "Storm Chaser".into(),
            title: "☁\u{FE0F} ✈\u{FE0F} ⌚\u{FE0E} 6 AM".into(),
            ..default_card()
        };
        let png = render_card(&data, 4).expect("render variation selectors");
        fs::write(format!("{dir}/10-variation-selectors.png"), &png).unwrap();

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
