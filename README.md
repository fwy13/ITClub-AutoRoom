# ITClub-AutoRoom

Rust/Axum + Tokio, SQLite, React/TypeScript/Tailwind CSS. Supports **Discussion Rooms A, B, C**, the other CTU library rooms, immediate/advance booking windows, authenticated CTU room cancellation, Telegram notifications, and automatic job recovery.

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

## 4. Reservation windows and persistent scheduler

All boundaries are **inclusive calendar days in Vietnam (UTC+07)**. Let **A = today**:

| Target date | Action | API |
| --- | --- | --- |
| A to A+13 | Attempt **immediately**. No booking/opening-time field required | `POST /api/bookings` |
| A+14 to A+44 (B=A+14, up to B+30) | Create a persistent advance job | `POST /api/jobs` with `kind: "prebook"` |
| Before A or after A+44 | Rejected | — |

Advance jobs accept `room_id`, `date` (`YYYY-MM-DD`), `slot`, `students`, `note`, `borrow_tv` and optional `kind: "prebook"`. **Do not send** `run_at` or `open_time_vn`: the app automatically computes a *checking start* of **00:00 ICT, 13 calendar days before the target date**. CTU's official booking-opening hour is **not established by the supplied HTML**; midnight does **not** guarantee bookings have opened. If the target slot is not visible/not open or the calendar is temporarily inaccessible, the scheduler rechecks at most every five minutes during its first eligible day. It never resends a POST that may already have reached CTU. An advance job is **not a confirmed booking**.

Existing manual/scheduled jobs from older versions are preserved and executed at their stored timestamps; the new API only creates advance jobs. `restart: always` restarts a crashed container when Docker is running. Pending/overdue jobs are retained in SQLite. Recovery safely requeues interrupted read-only checks, while possible CTU submissions become `uncertain`, never repeated blindly. The scheduler is deadline-based (30-second health backstop), not a 2-second poll. A powered-off VPS cannot execute jobs until restored.

### Cancel a confirmed CTU reservation

In **My bookings**, click **Cancel CTU reservation** on a confirmed booking and confirm. Alternatively, while signed in, call:

```http
POST /api/bookings/{booking_id}/cancel
```

The backend verifies the app-session owner, opens an authenticated CTU session with **that user's** stored CTU credentials, and sends **one** multipart/form-data request to `https://libapp.ctu.edu.vn/dkptl/huydk.php` containing:

```text
id_p=1
id_c=1
ngay_hoc=23-11-2026
```

These values are derived from the selected booking, not accepted as arbitrary user-supplied cancellation identifiers. Only CTU's exact `<div class='tb_ok'>Hủy thành công :)</div>` success notice releases the slot locally. A clear upstream error leaves the reservation confirmed; an ambiguous response or a crash during a potential POST becomes `cancel_uncertain` and **keeps the local slot occupied**. Never send a second cancellation automatically; check CTU's official website first.

`DELETE /api/jobs/{id}` only cancels a queued *advance job*, not a confirmed CTU reservation.

## 5. Apply the patch to an existing checkout

This patch was generated against the **exact source snapshot provided as Repomix XML**. It is not a patch against arbitrary revisions of the project. First commit or stash your own changes, then check compatibility **without modifying files**:

```bash
# Run from the repository root; point to the downloaded .patch file.
git apply --check /path/to/ITClub-AutoRoom-booking-redesign-v2.patch
# Only if the check exits with code 0:
git apply /path/to/ITClub-AutoRoom-booking-redesign-v2.patch
# Make a commit in your own repository:
git add README.md server web
git commit -m "feat: redesign reservation horizons and add CTU cancellation"
```

If you want Git to preserve the supplied commit message and author, use `git am /path/to/ITClub-AutoRoom-booking-redesign-v2.mbox` **instead of** `git apply` (not both). If you see `patch does not apply`, the current checkout differs from the uploaded snapshot, or the patch is already present. Check `git status` and `git apply --reverse --check <file>` before taking further action. Do not force the patch with `--reject` on a production deployment: use the full-source archive to compare and merge changes safely.

## 6. Update and preserve data

```bash
sudo docker compose up -d --build
sudo docker compose logs -f app
```

The SQLite migration `0006_cancellation.sql` is applied automatically on startup. Back up the database first. Keep `.env` (especially its encryption key) and the `app_data` Docker volume. **Do not use `docker compose down -v`.** If upgrading from a previous version using a different Compose project name, reuse the existing volume by setting `COMPOSE_PROJECT_NAME` to the old project name in `.env` before starting; never let two active stacks share the same SQLite database. Back up SQLite with its online backup API (or `VACUUM INTO`) and store the backup off-VPS.

## CTU empty slots vs bookable slots

The public CTU timetable can mark a shift `catrong_khl`: **empty, but not clickable in that view**. Older ITClub-AutoRoom versions incorrectly rejected every such shift as reserved/not open. The app now labels these slots **Empty (check CTU)** and does a second timetable check with the user's CTU login session before sending an attempt. `caban` (reserved), Sundays and expired/unknown cells remain blocked. CTU itself decides whether an empty shift is eligible at that time: an empty cell is **not** a guarantee of an accepted booking. Only CTU's explicit success notice marks a reservation confirmed. An ambiguous submission is not retried automatically.

Previously failed scheduled jobs (for example job #3) are not automatically replayed, since their original execution time has passed. Check the official CTU timetable and create a new attempt if the slot is still free and allowed. Rebuilding does not delete the SQLite volume.

## 7. Vietnamese UI and booking diagnostics

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
