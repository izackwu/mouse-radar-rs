# mouse-radar-rs

Telegram bot that monitors Strava athletes and notifies a group chat when they finish a workout, with weekly (Mon–Sun) and monthly mileage stats.

Uses the official [Strava API](https://developers.strava.com/) (OAuth) instead of web scraping.

## How it works

1. **Register** an athlete via `/register <name> <strava_id>` — generates an OAuth link
2. **Authorize** via `/auth <name> <strava_id> <code>` — exchanges the code for tokens
3. The bot **polls** the Strava API every N seconds for new activities
4. When a new Run, TrailRun, VirtualRun, Hike, or Walk is detected, it sends a message like:

```
🏃 zack ran 10.2 km
⏱ 4:52 /km · 49:42
📏 Week: 34.5 km · Month: 128.3 km
🔗 https://strava.com/activities/...
```

5. `/strava <name>` shows an athlete's current stats on demand

## Setup

### 1. Create a Strava API app

Go to [strava.com/settings/api](https://www.strava.com/settings/api) and create an app. The **Authorization Callback Domain** can be `localhost` — no website required.

### 2. Clone and configure

```bash
git clone <repo-url>
cd mouse-radar-rs
cp .env.example .env
```

Edit `.env`:

```env
TELEGRAM_BOT_TOKEN=<from @BotFather>
TELEGRAM_CHAT_ID=<group chat ID>
STRAVA_CLIENT_ID=<from Strava API settings>
STRAVA_CLIENT_SECRET=<from Strava API settings>
# Optional second Strava app — temporary workaround for Strava's
# 1-athlete-per-app default cap. See docs/superpowers/specs/
# 2026-06-01-multi-strava-clients-design.md for the full design and revert plan.
# STRAVA_CLIENT_ID_2=<from a second Strava API app>
# STRAVA_CLIENT_SECRET_2=<from a second Strava API app>
POLL_INTERVAL_SECONDS=300
COLD_START_LOOKBACK_DAYS=30
DATABASE_PATH=./data/bot.db
BOT_ADMIN_USERNAMES=<your_telegram_username>
TRACKED_ACTIVITY_TYPES=Run,TrailRun,VirtualRun,Hike,Walk
NOTIFICATION_MODE=card_and_text   # one of: card_only, text_only, card_and_text
```

### Multiple Strava apps (workaround)

Strava limits new API apps to 1 authorized athlete by default. To onboard a second athlete without waiting for a quota increase, create a second Strava app and set `STRAVA_CLIENT_ID_2` / `STRAVA_CLIENT_SECRET_2`. The bot pins each athlete to a slot at registration time — slot 1 (the first/required app) fills first, then slot 2. `/list` shows each athlete's slot, and `/register` tells you which one the OAuth link is for.

### 3. Run

```bash
cargo run
```

Or with Docker:

```bash
docker compose up -d
```

## Commands

| Command | Who | Purpose |
|---------|-----|---------|
| `/register <name> <strava_id>` | Admin | Generate OAuth link for an athlete |
| `/auth <name> <strava_id> <code>` | Admin | Exchange OAuth code for tokens |
| `/strava <name>` | Everyone | Show athlete's week/month stats |
| `/list` | Everyone | List all tracked athletes |
| `/help` | Everyone | Show available commands |

## Adding an athlete

1. Send `/register zack 96951505` to the bot
2. Bot replies with an OAuth URL — forward it to the athlete
3. Athlete clicks, authorizes, sees `localhost?code=...` redirect fail — copies the full URL
4. Send `/auth zack 96951505 <code>` to the bot
5. Done — the bot will discover their activities on the next poll cycle
