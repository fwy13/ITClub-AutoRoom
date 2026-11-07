# ITClub-AutoRoom

Rust/Axum + Tokio, SQLite, React/TypeScript/Tailwind CSS. Supports **Discussion Rooms A, B, C**, the other CTU library rooms, 13-day pre-booking, Telegram notifications, and automatic job recovery.

## 1. Deploy on a Linux VPS

Install Docker Engine and Docker Compose, then extract this project:

```bash
unzip ITClub-AutoRoom.zip
cd ITClub-AutoRoom
cp .env.example .env
openssl rand -hex 32
```

Paste the generated 64-character hex key into `APP_ENCRYPTION_KEY` in `.env` and keep a secure backup. For local HTTP use `COOKIE_SECURE=false` and `APP_ALLOWED_ORIGINS=http://localhost:3000` (the example defaults). For an HTTPS hostname, set both to its correct public origin and `true`.

```bash
sudo systemctl enable --now docker
sudo docker compose up -d --build
sudo docker compose ps
sudo docker compose logs --tail=100 -f app
```

Open `http://localhost:3000` on the VPS or forward its localhost port over SSH. Create an app account using only CTU credentials you are authorized to use. The uploaded HTML is an offline fixture; actual bookings require successful live CTU schedule checks.

## 2. Optional Cloudflare Tunnel

Create a tunnel and configure its public hostname to route to `http://app:3000`. In `.env`, set:

```dotenv
CLOUDFLARE_TUNNEL_TOKEN=YOUR_TOKEN
APP_ALLOWED_ORIGINS=https://rooms.example.com
COOKIE_SECURE=true
```

Then start both services:

```bash
sudo docker compose --profile tunnel up -d --build
sudo docker compose logs --tail=100 -f app cloudflared
```

## 3. Optional Telegram bot

Create a bot with BotFather and configure `TELEGRAM_BOT_TOKEN` and `TELEGRAM_BOT_USERNAME` in `.env`, then run `sudo docker compose up -d --force-recreate`. Users link the bot in the **Telegram & account** tab before receiving private booking notifications.

## 4. Schedule jobs and recovery

Use **Pre-book 13 days ahead** to select Day A, a room, shift, and the *actual* CTU opening hour in Vietnam time. SQLite stores the calculated Day A − 13 days timestamp. The event-driven Tokio scheduler sleeps until that deadline, wakes on job creation/cancellation, and runs a lightweight 30-second recovery/health check. It does **not** continuously poll SQLite every 2 seconds.

`restart: always` restarts a crashed container when Docker is running. On startup, pending and overdue jobs are considered; interrupted read-only checks are safely rescheduled. An attempt that might already have reached CTU is marked `uncertain`, **not** automatically submitted again. Booking success always requires CTU confirmation. A powered-off VPS cannot execute jobs until it comes back online. This deployment runs only **one** app instance with **one** SQLite volume.

## 5. Update and preserve data

```bash
sudo docker compose up -d --build
sudo docker compose logs -f app
```

Keep `.env` (especially its encryption key) and the `app_data` Docker volume. **Do not use `docker compose down -v`.** If upgrading from a previous version using a different Compose project name, reuse the existing volume by setting `COMPOSE_PROJECT_NAME` to the old project name in `.env` before starting; never let two active stacks share the same SQLite database. Back up SQLite with its online backup API (or `VACUUM INTO`) and store the backup off-VPS.

## CTU empty slots vs bookable slots

The public CTU timetable can mark a shift `catrong_khl`: **empty, but not clickable in that view**. Older ITClub-AutoRoom versions incorrectly rejected every such shift as reserved/not open. The app now labels these slots **Empty (check CTU)** and does a second timetable check with the user's CTU login session before sending an attempt. `caban` (reserved), Sundays and expired/unknown cells remain blocked. CTU itself decides whether an empty shift is eligible at that time: an empty cell is **not** a guarantee of an accepted booking. Only CTU's explicit success notice marks a reservation confirmed. An ambiguous submission is not retried automatically.

Previously failed scheduled jobs (for example job #3) are not automatically replayed, since their original execution time has passed. Check the official CTU timetable and create a new attempt if the slot is still free and allowed. Rebuilding does not delete the SQLite volume.

## 6. Vietnamese UI and booking diagnostics

The interface defaults to **Tiếng Việt**; use the `VI` / `EN` switch in the header. Language choice is saved in the current browser. Previously stored SQLite booking records and scheduled jobs are unaffected.

For detailed booking logs, add these lines to your existing `.env` (or update them), then restart the app:

```dotenv
RUST_LOG=itclub_autoroom=debug,tower_http=warn
RUST_BACKTRACE=1
```

```bash
sudo docker compose up -d --force-recreate app
sudo docker compose logs --since=1h --timestamps app | grep -E 'job_id=|stage=|ITClub-AutoRoom ready'
# To follow all logs without grep:
sudo docker compose logs -f --tail=200 --timestamps app
```

Follow `job_id` and `stage` to see `claimed` → `preflight` → `public_timetable` → `login` → `authenticated_timetable` → `submission` → `completed`. Each check logs the room ID, target date, shift and occupancy status where relevant; HTTP requests include status and elapsed milliseconds at DEBUG level. Only a confirmed CTU success notice reports booking success. Jobs already marked `failed` are **not** automatically replayed by logging changes; check the official CTU site before creating another attempt. Never share `.env`, Telegram tokens, session cookies, or unredacted private logs.
