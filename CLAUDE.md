# CLAUDE.md

Project notes for Claude Code (and other AI coding assistants).

## What this is

`mouse-radar-rs` is a Telegram bot that watches a set of Strava athletes and posts to a group chat when one of them finishes a workout. Polls the official Strava API (OAuth), no scraping. Stores athlete tokens and seen-activity IDs in a local SQLite file. Renders an "activity card" PNG for each notification.

## Architecture

```
src/
├── main.rs        # tokio runtime, wires teloxide bot + poller, dispatches commands
├── lib.rs         # re-exports the modules below so integration tests can import them
├── ai.rs          # AiClient trait + OpenAI-compatible client (any provider
│                  # speaking that wire format; selected by AI_BASE_URL/AI_MODEL)
├── config.rs      # env-driven Config; parsed once at startup
├── comment.rs     # AI activity comment: builds the grounded prompt from
│                  # activity detail + history, sanitizes, posts as a threaded
│                  # reply. Fully detached — never blocks a notification
├── db.rs          # rusqlite wrapper: athletes, seen_activities, activity_cache tables
│                  # (activity_cache powers the week/month stats on the card)
├── strava.rs      # StravaApi trait + StravaClient (reqwest). Trait exists for mocking
├── poller.rs      # background task: every N seconds, fetch new activities for each
│                  # athlete, refresh tokens if needed, send Telegram notifications.
│                  # Also listens on an mpsc channel for PollCommand (e.g. /auth
│                  # triggers an immediate ColdStart poll for that athlete)
├── commands.rs    # teloxide Command enum (typed args) + handlers (/register /auth
│                  # /strava /latest …); malformed commands get a usage-hint reply
├── formatting.rs  # pace, duration, distance formatters shared by message text + card
├── types.rs       # ActivityType enum (Run / TrailRun / Ride / Hike / Walk / Swim / Other)
└── card.rs        # SVG → PNG card renderer. The trickiest file in the repo — see below
```

Data flow for a notification:
1. `poller::run_poll_loop` fetches recent activities via `strava::StravaApi::list_activities`.
2. Any activity ID not in `seen_activities` is new — checked order-independently via `db::get_seen_ids` (never assume Strava's ordering; it's ascending with `after`, descending without). New tracked activities are cached in `activity_cache` and processed oldest-first.
3. `card::render_card` → PNG bytes; `commands::format_notification` → text + caption.
4. Send via `teloxide` to `TELEGRAM_CHAT_ID`. Mark the activity seen.

## Dev commands

```bash
cargo run                              # local bot (needs .env)
cargo test                             # unit + integration tests
cargo test --lib generate_snapshots    # regenerate card-snapshots/*.png on macOS
./scripts/gen-linux-snapshots.sh       # regenerate card-snapshots-linux/*.png in Docker
                                       # (Alpine + Noto CJK + Noto Color Emoji — matches prod)
cargo fmt --check                      # format check
cargo clippy --all-targets --all-features -- -D warnings   # lints (CI gates on this)
```

A pre-commit hook in `.githooks/pre-commit` (wired up via `git config core.hooksPath .githooks`) runs `cargo fmt --check`, `cargo check`, and `cargo clippy --all-targets --all-features -- -D warnings`. Don't bypass it with `--no-verify`.

## Conventions

- **Commits**: conventional commits, one-line message most of the time (`feat:`, `fix:`, `refactor:`, `docs:`, `ci:`). No co-author trailers.
- **Errors**: `anyhow::Result` everywhere except library boundaries. Use `?` aggressively; only convert to a user-facing string at the Telegram message layer.
- **Async**: `tokio` everywhere. `Arc<dyn Trait>` for the things shared between the poller task and the command handlers (db, strava client).
- **Logging**: `log` macros (`info!`, `debug!`, `warn!`, `error!`) — initialized via `env_logger`, default filter `info`.
- **Mocking**: `strava::StravaApi` is a trait so tests can swap in a fake. Apply the same pattern when adding external services.
- **Clippy**: `pedantic` is enabled at warn level. Selectively `#[allow(...)]` only when there's a real reason — see `Cargo.toml` for the project-wide opt-outs.

## Card rendering (`src/card.rs`)

The highest-tribal-knowledge area of the codebase: SVG template → `usvg` parse (shared `FONTS` fontdb) → `resvg` render → PNG. Font fallback order, input sanitization, known usvg limitations, and the snapshot-test workflow are documented in [docs/card-rendering.md](docs/card-rendering.md) — **read it before touching `card.rs`, the `FONTS` setup, or the Dockerfile fonts**, and re-run the snapshot tests on both macOS and Linux after any change there.

## Deployment

Production runs in Docker on a tiny VPS, image pushed to `ghcr.io/izackwu/mouse-radar-rs:latest` by `.github/workflows/build.yml` on every push to `main`. The runtime image is Alpine 3.21 with `ca-certificates`, `font-noto-cjk`, and `font-noto-emoji` — the CJK + emoji fonts are required for the card renderer's fallback chain to find anything on Linux. `compose.yml` wires the SQLite volume.

CI (`.github/workflows/ci.yml`) runs fmt + clippy + test on every PR.

## Things to be careful about

- **Don't drop `font-noto-cjk` / `font-noto-emoji` from the Dockerfile** — the Linux build will silently regress to tofu boxes for everything past Latin-1 (see [docs/card-rendering.md](docs/card-rendering.md)).
- **Strava's `start_date_local` is mislabeled with a `Z` suffix** — it's local time wearing a UTC costume. Use `start_date` (true UTC) for any cutoff/epoch math; deriving cutoffs from `start_date_local` jumps hours into the future in positive-offset zones and drops same-day activities (see the fix in #14).
- **Don't add a CLAUDE.md, README.md, or other doc file unsolicited** — only when the user asks. Same for `// removed X` comments and re-exports for backwards compat.
- **The `card-snapshots/` and `card-snapshots-linux/` directories are gitignored**. Don't commit them. They're regenerated locally.
- **Strava OAuth refresh tokens** are stored in plaintext in SQLite. Rotation is automatic on 401. Don't log them.
- **The AI comment is sent as plain text with no `parse_mode`** — the card caption
  uses MarkdownV2, which requires escaping `.` `-` `(` `)` `!` `+`. Model output is
  full of those, and send failures are swallowed, so reusing that parse mode fails
  silently on nearly every message.
- **Don't log `AI_API_KEY`** — same rule as Strava refresh tokens. `AiConfig` has a
  hand-written `Debug` impl that redacts it, because `Config` derives `Debug` and is
  logged at startup.
