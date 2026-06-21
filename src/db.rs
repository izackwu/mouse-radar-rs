use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use crate::types::ActivityType;
use std::str::FromStr;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;",
        )?;
        init_schema(&conn)?;
        migrate_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Run a closure with the connection, blocks on the mutex.
    /// Call from `spawn_blocking` in async contexts.
    pub fn run<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self.conn.lock().unwrap();
        f(&guard)
    }

    /// Build text message, card PNG, and caption for an activity.
    ///
    /// Queries week/month stats from the database automatically.
    pub fn build_notification(
        &self,
        athlete_name: &str,
        activity: &CachedActivity,
    ) -> anyhow::Result<crate::card::Notification> {
        self.run(|conn| {
            let (monday, first_of_month) = period_boundaries();
            let week = get_week_km(conn, activity.athlete_id, monday)?;
            let month = get_month_km(conn, activity.athlete_id, first_of_month)?;
            let oldest = get_oldest_activity_date(conn, activity.athlete_id)?;
            let (inc_week, inc_month) = crate::formatting::incomplete_periods(oldest);

            let text = crate::formatting::format_activity_message(
                athlete_name,
                &activity.title,
                activity.activity_type,
                activity.distance_km,
                activity.pace_sec_per_km,
                activity.duration_s,
                week,
                month,
                &activity.url,
                inc_week,
                inc_month,
            );

            let card_data = crate::card::CardData {
                activity_type: activity.activity_type,
                athlete_name: athlete_name.to_string(),
                title: activity.title.clone(),
                start_date_local: activity.start_date_local.clone(),
                distance_km: activity.distance_km,
                pace_sec_per_km: activity.pace_sec_per_km,
                duration_s: activity.duration_s,
                week_km: week,
                month_km: month,
                incomplete_week: inc_week,
                incomplete_month: inc_month,
            };

            let card_png = crate::card::render_card(&card_data, 4)?;
            let caption = crate::card::format_caption(athlete_name, &activity.url);
            Ok(crate::card::Notification {
                text,
                card_png,
                caption,
            })
        })
    }
}

pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS athletes (
            strava_id     INTEGER PRIMARY KEY,
            name          TEXT NOT NULL,
            access_token  TEXT NOT NULL,
            refresh_token TEXT NOT NULL,
            token_expires INTEGER NOT NULL,
            added_at      INTEGER NOT NULL DEFAULT (unixepoch())
        );

        CREATE TABLE IF NOT EXISTS seen_activities (
            activity_id  INTEGER PRIMARY KEY,
            athlete_id   INTEGER NOT NULL REFERENCES athletes(strava_id),
            notified_at  INTEGER NOT NULL DEFAULT (unixepoch())
        );

        CREATE TABLE IF NOT EXISTS activity_cache (
            activity_id     INTEGER PRIMARY KEY,
            athlete_id      INTEGER NOT NULL REFERENCES athletes(strava_id),
            title           TEXT,
            activity_type   TEXT,
            distance_km     REAL,
            duration_s      INTEGER,
            pace_sec_per_km INTEGER,
            start_date_local TEXT,     -- ISO 8601 datetime in athlete's local timezone
            start_date       TEXT,     -- ISO 8601 datetime in true UTC (Strava's start_date)
            url             TEXT,
            cached_at       INTEGER NOT NULL DEFAULT (unixepoch())
        );",
    )?;
    Ok(())
}

/// Idempotent migrations for databases created before a column existed.
pub fn migrate_schema(conn: &Connection) -> Result<()> {
    if !column_exists(conn, "activity_cache", "start_date")? {
        conn.execute_batch("ALTER TABLE activity_cache ADD COLUMN start_date TEXT;")?;
    }
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Clone)]
pub struct Athlete {
    pub strava_id: i64,
    pub name: String,
    pub access_token: String,
    pub refresh_token: String,
    pub token_expires: i64,
    pub added_at: i64,
}

pub fn insert_athlete(
    conn: &Connection,
    strava_id: i64,
    name: &str,
    access_token: &str,
    refresh_token: &str,
    token_expires: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO athletes (strava_id, name, access_token, refresh_token, token_expires)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![strava_id, name, access_token, refresh_token, token_expires],
    )?;
    Ok(())
}

pub fn get_athlete(conn: &Connection, strava_id: i64) -> Result<Option<Athlete>> {
    let mut stmt = conn.prepare(
        "SELECT strava_id, name, access_token, refresh_token, token_expires, added_at
         FROM athletes WHERE strava_id = ?1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![strava_id], |row| {
        Ok(Athlete {
            strava_id: row.get(0)?,
            name: row.get(1)?,
            access_token: row.get(2)?,
            refresh_token: row.get(3)?,
            token_expires: row.get(4)?,
            added_at: row.get(5)?,
        })
    })?;
    match rows.next() {
        Some(result) => Ok(Some(result?)),
        None => Ok(None),
    }
}

pub fn list_athletes(conn: &Connection) -> Result<Vec<Athlete>> {
    let mut stmt = conn.prepare(
        "SELECT strava_id, name, access_token, refresh_token, token_expires, added_at
         FROM athletes ORDER BY added_at",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Athlete {
            strava_id: row.get(0)?,
            name: row.get(1)?,
            access_token: row.get(2)?,
            refresh_token: row.get(3)?,
            token_expires: row.get(4)?,
            added_at: row.get(5)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn update_athlete_tokens(
    conn: &Connection,
    strava_id: i64,
    access_token: &str,
    refresh_token: &str,
    token_expires: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE athletes SET access_token = ?1, refresh_token = ?2, token_expires = ?3
         WHERE strava_id = ?4",
        rusqlite::params![access_token, refresh_token, token_expires, strava_id],
    )?;
    Ok(())
}

// --- Seen activities ---

pub fn is_activity_seen(conn: &Connection, activity_id: i64) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT COUNT(*) FROM seen_activities WHERE activity_id = ?1")?;
    let count: i64 = stmt.query_row(rusqlite::params![activity_id], |row| row.get(0))?;
    Ok(count > 0)
}

/// Subset of `ids` that are already in `seen_activities`.
pub fn get_seen_ids(conn: &Connection, ids: &[i64]) -> Result<HashSet<i64>> {
    let mut stmt = conn.prepare("SELECT 1 FROM seen_activities WHERE activity_id = ?1")?;
    let mut seen = HashSet::new();
    for &id in ids {
        if stmt.exists([id])? {
            seen.insert(id);
        }
    }
    Ok(seen)
}

pub fn mark_activity_seen(conn: &Connection, activity_id: i64, athlete_id: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO seen_activities (activity_id, athlete_id) VALUES (?1, ?2)",
        rusqlite::params![activity_id, athlete_id],
    )?;
    Ok(())
}

pub fn bulk_mark_seen(conn: &Connection, items: &[(i64, i64)]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO seen_activities (activity_id, athlete_id) VALUES (?1, ?2)",
    )?;
    for (activity_id, athlete_id) in items {
        stmt.execute(rusqlite::params![activity_id, athlete_id])?;
    }
    Ok(())
}

// --- Activity cache ---

#[derive(Debug, Clone)]
pub struct CachedActivity {
    pub activity_id: i64,
    pub athlete_id: i64,
    pub title: String,
    pub activity_type: ActivityType,
    pub distance_km: f64,
    pub duration_s: i64,
    pub pace_sec_per_km: Option<i64>,
    pub start_date_local: String,
    /// Strava's `start_date` — the true UTC instant the activity began.
    pub start_date: String,
    pub url: String,
}

pub fn cache_activity(conn: &Connection, activity: &CachedActivity) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO activity_cache
         (activity_id, athlete_id, title, activity_type, distance_km, duration_s,
          pace_sec_per_km, start_date_local, start_date, url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            activity.activity_id,
            activity.athlete_id,
            activity.title,
            activity.activity_type.to_string(),
            activity.distance_km,
            activity.duration_s,
            activity.pace_sec_per_km,
            activity.start_date_local,
            activity.start_date,
            activity.url,
        ],
    )?;
    Ok(())
}

pub fn bulk_cache_activities(conn: &Connection, activities: &[CachedActivity]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR REPLACE INTO activity_cache
         (activity_id, athlete_id, title, activity_type, distance_km, duration_s,
          pace_sec_per_km, start_date_local, start_date, url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for a in activities {
        stmt.execute(rusqlite::params![
            a.activity_id,
            a.athlete_id,
            a.title,
            a.activity_type.to_string(),
            a.distance_km,
            a.duration_s,
            a.pace_sec_per_km,
            a.start_date_local,
            a.start_date,
            a.url,
        ])?;
    }
    Ok(())
}

/// Returns (monday, `first_of_month`) in the server's local timezone.
#[must_use]
pub fn period_boundaries() -> (NaiveDate, NaiveDate) {
    let today = Local::now().date_naive();
    let monday = today - chrono::Duration::days(i64::from(today.weekday().num_days_from_monday()));
    let first_of_month =
        NaiveDate::from_ymd_opt(today.year(), today.month(), 1).expect("valid date");
    (monday, first_of_month)
}

pub fn get_week_km(conn: &Connection, athlete_id: i64, monday: NaiveDate) -> Result<f64> {
    let monday_str = monday.format("%Y-%m-%d").to_string();
    let next_monday = monday + chrono::Duration::days(7);
    let next_str = next_monday.format("%Y-%m-%d").to_string();
    let total: f64 = conn.query_row(
        "SELECT COALESCE(SUM(distance_km), 0.0) FROM activity_cache
         WHERE athlete_id = ?1
           AND date(start_date_local) >= ?2
           AND date(start_date_local) <  ?3",
        rusqlite::params![athlete_id, monday_str, next_str],
        |row| row.get(0),
    )?;
    Ok(total)
}

pub fn get_month_km(conn: &Connection, athlete_id: i64, first_of_month: NaiveDate) -> Result<f64> {
    let month_str = first_of_month.format("%Y-%m-%d").to_string();
    let next_month = if first_of_month.month() == 12 {
        NaiveDate::from_ymd_opt(first_of_month.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(first_of_month.year(), first_of_month.month() + 1, 1)
    }
    .expect("valid date");
    let next_str = next_month.format("%Y-%m-%d").to_string();
    let total: f64 = conn.query_row(
        "SELECT COALESCE(SUM(distance_km), 0.0) FROM activity_cache
         WHERE athlete_id = ?1
           AND date(start_date_local) >= ?2
           AND date(start_date_local) <  ?3",
        rusqlite::params![athlete_id, month_str, next_str],
        |row| row.get(0),
    )?;
    Ok(total)
}

pub fn get_oldest_activity_date(conn: &Connection, athlete_id: i64) -> Result<Option<NaiveDate>> {
    let text: Option<String> = conn.query_row(
        "SELECT MIN(start_date_local) FROM activity_cache WHERE athlete_id = ?1",
        rusqlite::params![athlete_id],
        |row| row.get(0),
    )?;
    Ok(text
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d").ok()))
}

pub fn get_latest_activity(conn: &Connection, athlete_id: i64) -> Result<Option<CachedActivity>> {
    let mut stmt = conn.prepare(
        "SELECT activity_id, athlete_id, title, activity_type, distance_km, duration_s,
                pace_sec_per_km, start_date_local, start_date, url
         FROM activity_cache
         WHERE athlete_id = ?1
         ORDER BY start_date_local DESC, activity_id DESC
         LIMIT 1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![athlete_id], |row| {
        Ok(CachedActivity {
            activity_id: row.get(0)?,
            athlete_id: row.get(1)?,
            title: row.get(2)?,
            activity_type: {
                let at_str: String = row.get(3)?;
                ActivityType::from_str(&at_str).unwrap_or(ActivityType::Other)
            },
            distance_km: row.get(4)?,
            duration_s: row.get(5)?,
            pace_sec_per_km: row.get(6)?,
            start_date_local: row.get(7)?,
            start_date: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
            url: row.get(9)?,
        })
    })?;
    match rows.next() {
        Some(result) => Ok(Some(result?)),
        None => Ok(None),
    }
}

/// Most recent true-UTC `start_date` across an athlete's cached activities.
///
/// Used to compute the Strava `after` poll cutoff. Must be UTC, not
/// `start_date_local` (which Strava mislabels with a `Z`): using local time
/// pushes the cutoff hours into the future in positive-offset zones and
/// silently drops same-day activities that start after an earlier one.
pub fn get_last_activity_utc(conn: &Connection, athlete_id: i64) -> Result<Option<String>> {
    Ok(conn.query_row(
        "SELECT MAX(start_date) FROM activity_cache
         WHERE athlete_id = ?1 AND start_date IS NOT NULL AND start_date != ''",
        rusqlite::params![athlete_id],
        |row| row.get(0),
    )?)
}

/// Whether the athlete has any cached activity. Drives cold-start detection
/// independently of `get_last_activity_utc`, which returns None for legacy rows
/// (cached before `start_date` existed) and must not be mistaken for a fresh athlete.
pub fn has_cached_activities(conn: &Connection, athlete_id: i64) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM activity_cache WHERE athlete_id = ?1",
        rusqlite::params![athlete_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;
    use tempfile::tempdir;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        Db::open(dir.path().join("test.db").to_str().unwrap()).unwrap()
    }

    #[test]
    fn test_migrate_adds_start_date_to_legacy_cache() {
        // Simulate a database created before the start_date column existed.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE activity_cache (
                activity_id      INTEGER PRIMARY KEY,
                athlete_id       INTEGER NOT NULL,
                title            TEXT,
                activity_type    TEXT,
                distance_km      REAL,
                duration_s       INTEGER,
                pace_sec_per_km  INTEGER,
                start_date_local TEXT,
                url              TEXT,
                cached_at        INTEGER NOT NULL DEFAULT (unixepoch())
            );
            INSERT INTO activity_cache (activity_id, athlete_id, start_date_local)
            VALUES (1, 1, '2024-01-01T08:00:00Z');",
        )
        .unwrap();

        assert!(!column_exists(&conn, "activity_cache", "start_date").unwrap());

        init_schema(&conn).unwrap();
        migrate_schema(&conn).unwrap();

        assert!(column_exists(&conn, "activity_cache", "start_date").unwrap());
        // Legacy row's start_date is NULL, so the UTC cutoff falls back to None.
        assert!(get_last_activity_utc(&conn, 1).unwrap().is_none());
        // Migration is idempotent.
        migrate_schema(&conn).unwrap();
    }

    #[test]
    fn test_init_schema_creates_tables() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let db = Db::open(db_path.to_str().unwrap()).unwrap();

        db.run(|conn| {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .unwrap();
            let tables: Vec<String> = stmt
                .query_map([], |row| row.get(0))
                .unwrap()
                .filter_map(Result::ok)
                .collect();

            assert_eq!(
                tables,
                vec!["activity_cache", "athletes", "seen_activities",]
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_insert_and_get_athlete() {
        let db = test_db();

        db.run(|conn| {
            insert_athlete(conn, 12345, "alice", "acc_tok", "ref_tok", 1_710_000_000).unwrap();

            let a = get_athlete(conn, 12345).unwrap().unwrap();
            assert_eq!(a.strava_id, 12345);
            assert_eq!(a.name, "alice");
            assert_eq!(a.access_token, "acc_tok");
            assert_eq!(a.refresh_token, "ref_tok");
            assert_eq!(a.token_expires, 1_710_000_000);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_list_athletes() {
        let db = test_db();

        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();
            insert_athlete(conn, 2, "bob", "a", "r", 0).unwrap();

            let list = list_athletes(conn).unwrap();
            assert_eq!(list.len(), 2);
            assert_eq!(list[0].name, "alice");
            assert_eq!(list[1].name, "bob");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_update_athlete_tokens() {
        let db = test_db();

        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "old_acc", "old_ref", 100).unwrap();
            update_athlete_tokens(conn, 1, "new_acc", "new_ref", 200).unwrap();

            let a = get_athlete(conn, 1).unwrap().unwrap();
            assert_eq!(a.access_token, "new_acc");
            assert_eq!(a.refresh_token, "new_ref");
            assert_eq!(a.token_expires, 200);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_get_nonexistent_athlete() {
        let db = test_db();

        db.run(|conn| {
            let a = get_athlete(conn, 999).unwrap();
            assert!(a.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_seen_activities() {
        let db = test_db();

        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();

            assert!(!is_activity_seen(conn, 100).unwrap());
            mark_activity_seen(conn, 100, 1).unwrap();
            assert!(is_activity_seen(conn, 100).unwrap());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_bulk_mark_seen() {
        let db = test_db();

        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();

            bulk_mark_seen(conn, &[(101, 1), (102, 1), (103, 1)]).unwrap();
            assert!(is_activity_seen(conn, 101).unwrap());
            assert!(is_activity_seen(conn, 102).unwrap());
            assert!(is_activity_seen(conn, 103).unwrap());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_get_seen_ids() {
        let db = test_db();

        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();
            bulk_mark_seen(conn, &[(10, 1), (11, 1)]).unwrap();

            let set = get_seen_ids(conn, &[10, 11, 12]).unwrap();
            assert_eq!(set.len(), 2);
            assert!(set.contains(&10));
            assert!(set.contains(&11));
            assert!(!set.contains(&12));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_cache_and_retrieve_activity() {
        let db = test_db();

        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();

            let act = CachedActivity {
                activity_id: 200,
                athlete_id: 1,
                title: "Morning Run".into(),
                activity_type: ActivityType::Run,
                distance_km: 10.5,
                duration_s: 3000,
                pace_sec_per_km: Some(286),
                start_date_local: "2024-01-15T08:30:00Z".into(),
                start_date: "2024-01-15T08:30:00Z".into(),
                url: "https://strava.com/activities/200".into(),
            };
            cache_activity(conn, &act).unwrap();

            let latest = get_latest_activity(conn, 1).unwrap().unwrap();
            assert_eq!(latest.title, "Morning Run");
            assert_eq!(latest.distance_km, 10.5);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_week_month_km() {
        let db = test_db();

        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();

            // Use fixed dates within the same week/month
            let date1 = "2026-05-14T08:00:00"; // Thursday
            let date2 = "2026-05-15T16:00:00"; // Friday same week

            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 1,
                    athlete_id: 1,
                    title: "R1".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 5.0,
                    duration_s: 1800,
                    pace_sec_per_km: None,
                    start_date_local: date1.into(),
                    start_date: String::new(),
                    url: String::new(),
                },
            )
            .unwrap();
            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 2,
                    athlete_id: 1,
                    title: "R2".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 7.5,
                    duration_s: 2700,
                    pace_sec_per_km: None,
                    start_date_local: date2.into(),
                    start_date: String::new(),
                    url: String::new(),
                },
            )
            .unwrap();

            // Monday of that week is 2026-05-11
            let monday = NaiveDate::from_ymd_opt(2026, 5, 11).unwrap();
            let first_of_month = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();

            let week = get_week_km(conn, 1, monday).unwrap();
            let month = get_month_km(conn, 1, first_of_month).unwrap();
            assert_eq!(week, 12.5);
            assert_eq!(month, 12.5);

            // Activity before Monday should not be counted
            let week_before =
                get_week_km(conn, 1, NaiveDate::from_ymd_opt(2026, 5, 18).unwrap()).unwrap();
            assert_eq!(week_before, 0.0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_week_boundary_sunday_night() {
        // Activity at 23:00 Sunday local time — counts for the week ending that Sunday
        let db = test_db();
        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();

            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 1,
                    athlete_id: 1,
                    title: "Sun Night".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 5.0,
                    duration_s: 1800,
                    pace_sec_per_km: None,
                    start_date_local: "2026-05-10T23:00:00".into(), // Sunday
                    start_date: String::new(),
                    url: String::new(),
                },
            )
            .unwrap();

            // Monday of that week (May 4) includes Sunday May 10
            let monday = NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
            assert_eq!(get_week_km(conn, 1, monday).unwrap(), 5.0);

            // Monday of next week (May 11) does NOT include Sunday May 10
            let next_monday = NaiveDate::from_ymd_opt(2026, 5, 11).unwrap();
            assert_eq!(get_week_km(conn, 1, next_monday).unwrap(), 0.0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_week_boundary_monday_morning() {
        // Activity at 00:01 Monday local time — counts for the new week
        let db = test_db();
        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();

            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 1,
                    athlete_id: 1,
                    title: "Mon Morning".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 5.0,
                    duration_s: 1800,
                    pace_sec_per_km: None,
                    start_date_local: "2026-05-11T00:01:00".into(), // Monday
                    start_date: String::new(),
                    url: String::new(),
                },
            )
            .unwrap();

            // Should be in the week starting May 11
            let monday = NaiveDate::from_ymd_opt(2026, 5, 11).unwrap();
            assert_eq!(get_week_km(conn, 1, monday).unwrap(), 5.0);

            // Should NOT be in the week starting May 4
            let prev_monday = NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
            assert_eq!(get_week_km(conn, 1, prev_monday).unwrap(), 0.0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_month_boundary() {
        // Month-end: May 31 vs Jun 1
        let db = test_db();
        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();

            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 1,
                    athlete_id: 1,
                    title: "May Run".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 10.0,
                    duration_s: 3600,
                    pace_sec_per_km: None,
                    start_date_local: "2026-05-31T22:00:00".into(),
                    start_date: String::new(),
                    url: String::new(),
                },
            )
            .unwrap();
            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 2,
                    athlete_id: 1,
                    title: "Jun Run".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 3.0,
                    duration_s: 900,
                    pace_sec_per_km: None,
                    start_date_local: "2026-06-01T06:00:00".into(),
                    start_date: String::new(),
                    url: String::new(),
                },
            )
            .unwrap();

            // May total
            let may_1 = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
            assert_eq!(get_month_km(conn, 1, may_1).unwrap(), 10.0);

            // June total
            let jun_1 = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
            assert_eq!(get_month_km(conn, 1, jun_1).unwrap(), 3.0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_date_extraction_ignores_time() {
        // `date(start_date_local)` extracts only the date part, ignoring time
        let db = test_db();
        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();

            // Same date, different times — should all be included
            for (time_str, activity_id_val) in [
                ("2026-05-14T00:00:01", 1_i64),
                ("2026-05-14T12:00:00", 2),
                ("2026-05-14T23:59:59", 3),
            ] {
                cache_activity(
                    conn,
                    &CachedActivity {
                        activity_id: activity_id_val,
                        athlete_id: 1,
                        title: "Run".into(),
                        activity_type: ActivityType::Run,
                        distance_km: 2.0,
                        duration_s: 600,
                        pace_sec_per_km: None,
                        start_date_local: time_str.into(),
                        start_date: String::new(),
                        url: String::new(),
                    },
                )
                .unwrap();
            }

            // All three count for the week containing May 14
            let monday = NaiveDate::from_ymd_opt(2026, 5, 11).unwrap();
            assert_eq!(get_week_km(conn, 1, monday).unwrap(), 6.0);

            // Previous day (May 13, 11:59pm) does NOT count for week starting May 11
            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 0,
                    athlete_id: 1,
                    title: "Late".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 1.0,
                    duration_s: 300,
                    pace_sec_per_km: None,
                    start_date_local: "2026-05-10T23:59:00".into(),
                    start_date: String::new(),
                    url: String::new(),
                },
            )
            .unwrap();
            // This is in the previous week (Mon May 4), not this week (Mon May 11)
            assert_eq!(get_week_km(conn, 1, monday).unwrap(), 6.0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_last_and_oldest_activity_date() {
        let db = test_db();

        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();

            assert!(get_last_activity_utc(conn, 1).unwrap().is_none());
            assert!(get_oldest_activity_date(conn, 1).unwrap().is_none());

            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 1,
                    athlete_id: 1,
                    title: "Old".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 5.0,
                    duration_s: 1800,
                    pace_sec_per_km: None,
                    start_date_local: "2024-01-01T00:00:00Z".into(),
                    start_date: "2024-01-01T00:00:00Z".into(),
                    url: String::new(),
                },
            )
            .unwrap();
            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 2,
                    athlete_id: 1,
                    title: "New".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 5.0,
                    duration_s: 1800,
                    pace_sec_per_km: None,
                    start_date_local: "2024-01-15T00:00:00Z".into(),
                    start_date: "2024-01-15T00:00:00Z".into(),
                    url: String::new(),
                },
            )
            .unwrap();

            assert_eq!(
                get_last_activity_utc(conn, 1).unwrap().unwrap(),
                "2024-01-15T00:00:00Z"
            );
            assert_eq!(
                get_oldest_activity_date(conn, 1).unwrap().unwrap(),
                NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_last_activity_utc_uses_true_utc_not_local() {
        // Strava labels start_date_local with a misleading `Z`. The poll `after`
        // cutoff must come from the true-UTC start_date — using local time pushes
        // the cutoff hours ahead in positive-offset zones, silently dropping
        // same-day activities that start after an earlier one.
        let db = test_db();
        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();
            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 1,
                    athlete_id: 1,
                    title: "Warmup".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 1.0,
                    duration_s: 500,
                    pace_sec_per_km: None,
                    start_date_local: "2026-06-14T07:59:51Z".into(), // AEST wall clock
                    start_date: "2026-06-13T21:59:51Z".into(),       // true UTC, 10h earlier
                    url: String::new(),
                },
            )
            .unwrap();

            assert_eq!(
                get_last_activity_utc(conn, 1).unwrap().unwrap(),
                "2026-06-13T21:59:51Z"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_has_cached_activities() {
        let db = test_db();
        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();
            assert!(!has_cached_activities(conn, 1).unwrap());
            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 1,
                    athlete_id: 1,
                    title: "Run".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 5.0,
                    duration_s: 1800,
                    pace_sec_per_km: None,
                    start_date_local: "2024-01-01T08:00:00Z".into(),
                    start_date: String::new(),
                    url: String::new(),
                },
            )
            .unwrap();
            assert!(has_cached_activities(conn, 1).unwrap());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_last_activity_utc_ignores_legacy_rows_without_start_date() {
        // Rows cached before the start_date column existed have NULL start_date.
        // The cutoff must ignore them (returning None so the poller falls back to
        // its lookback window) rather than treating them as the epoch 0 / empty.
        let db = test_db();
        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();
            cache_activity(
                conn,
                &CachedActivity {
                    activity_id: 1,
                    athlete_id: 1,
                    title: "Legacy".into(),
                    activity_type: ActivityType::Run,
                    distance_km: 5.0,
                    duration_s: 1800,
                    pace_sec_per_km: None,
                    start_date_local: "2024-01-01T08:00:00Z".into(),
                    start_date: String::new(),
                    url: String::new(),
                },
            )
            .unwrap();

            assert!(get_last_activity_utc(conn, 1).unwrap().is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_bulk_cache_activities() {
        let db = test_db();

        db.run(|conn| {
            insert_athlete(conn, 1, "alice", "a", "r", 0).unwrap();

            let today = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

            bulk_cache_activities(
                conn,
                &[
                    CachedActivity {
                        activity_id: 1,
                        athlete_id: 1,
                        title: "A".into(),
                        activity_type: ActivityType::Run,
                        distance_km: 1.0,
                        duration_s: 100,
                        pace_sec_per_km: None,
                        start_date_local: today.clone(),
                        start_date: today.clone(),
                        url: String::new(),
                    },
                    CachedActivity {
                        activity_id: 2,
                        athlete_id: 1,
                        title: "B".into(),
                        activity_type: ActivityType::Run,
                        distance_km: 2.0,
                        duration_s: 200,
                        pace_sec_per_km: None,
                        start_date_local: today.clone(),
                        start_date: today.clone(),
                        url: String::new(),
                    },
                ],
            )
            .unwrap();

            let a = get_latest_activity(conn, 1).unwrap().unwrap();
            assert_eq!(a.activity_id, 2); // latest by start_date (ties: higher id)
            Ok(())
        })
        .unwrap();
    }
}
