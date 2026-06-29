# arca — architecture

Codebase map — current state, read off the code. Per-provider detail is in
`providers.md` (authoritative); the threat posture is in `threat-model.md`;
non-negotiable rules + platform constraints are in the project `the design spec`;
current focus/status is the driver, `../../plan.md`.

## Process model

One daemon, two clients, one bridge.

- **`arca-daemon`** (`crates/arca-daemon`) — the single long-running binary.
  Supervised by `rc.d/arca`, runs as `_arca`.
- **`arca`** (`crates/arca-tui`) — the TUI *and* the one-shot CLI, same binary.
  No subcommand → TUI; a verb (`money`, `debt`, `pp`, `tx`, `recurring`,
  `health`, `alerts`, `export`, …) → one JSON reply and exit.
- **`arca-xmpp.py`** (`bridge/`) — Python (slixmpp) bridge on the router,
  supervised by `rc.d/arca_xmpp`; not in the workspace. Talks to the daemon over
  the read socket and pushes alerts/reports. See the design spec "Operator channel".

```
arca-daemon (tokio, 2 worker threads)
├── read.sock   /var/run/arca/read.sock   0660  read verbs only,  no UID gate
├── write.sock  /var/run/arca/write.sock  0660  all verbs, getpeereid == operator_uid
├── tcp         127.0.0.1:7732            all verbs, no UID gate, loopback-enforced, latent
├── scheduler   per-provider refresh (60s tick; cadence vs last_poll_at)
├── alert engine    → alert_history (delivered=0)
├── reports engine  → <reports_dir>/<YYYY-MM>.md
├── calendar engine → <ics_dir>/arca-YYYYMMDD.ics
└── SQLite /var/arca/arca.db (WAL, foreign_keys=ON)
```

## Crates

- **`arca-core`** — shared, no I/O beyond SQLite. `money` (`Cents`), `ids`,
  `time` (unix↔AST, `parse_ymd`), `error` (`CoreError`), `db` (rusqlite +
  embedded migrations), `provider` (the `Provider` trait + `Ctx`/`RefreshReport`),
  `rpc` (wire `Request`/`Response`, length-prefixed JSON framing, `is_write()`),
  and the read-model engines: `pp` (T2 drift + T1 backbone), `debt`, `recurring`
  (series detection + `labeled_series`), `report` (monthly Markdown), `calendar`
  (hand-rolled RFC 5545 `.ics`).
- **`arca-daemon`** — the daemon binary. `config` (TOML), `secrets` (age →
  in-memory, zeroized), `pledge`/`peercred` (sandbox + getpeereid), `log_writer`
  (SIGUSR1 reopen), `http` (one shared rustls `reqwest::Client`), `scheduler`,
  `alerts`, `reports`, `calendar` (engine tasks), `rpc/` (`server` listeners +
  role gate, `handler` dispatch), `providers/` (`registry` factory + one module
  per provider + `convert`).
- **`arca-tui`** — the `arca` binary. `main` (clap verbs, one-shot/cmd/smoke/TUI
  dispatch, CSV export), `app` (the `View` state machine + ratatui rendering +
  `refresh_*` RPC calls), `client` (RPC client over Unix or TCP).

## Startup (`arca-daemon` main)

1. Load TOML config (`--conf`, default `/etc/arca/arca.conf`).
2. Init tracing (file via `tracing-appender` if the log dir exists, else stderr).
3. Write PID file *before* the sandbox, so root `newsyslog` can signal it.
4. Decrypt `secrets.age` into memory.
5. Open the DB → WAL + foreign keys + apply embedded migrations (idempotent via
   a `_migrations` table).
6. Build the provider registry from `providers` rows (seeds the `manual` row).
7. **`unveil` then `pledge`** (no-op off OpenBSD), then bind listeners *eagerly*
   so a bad bind aborts before tasks spawn.
8. Spawn read/write/tcp listeners + scheduler + alert/reports/calendar engines.
9. Signal loop: SIGTERM/SIGINT → drain + remove PID; **SIGHUP → restart-only
   notice** (no hot reload in v1); SIGUSR1 → reopen the log file.

## RPC

Length-prefixed (u32 LE) JSON, one request per connection. Types + framing live
in `arca-core/src/rpc.rs` (authoritative); `Request::is_write()` tags mutating
verbs. Two Unix sockets split the surface:

- **read.sock** — `snapshot.money|business|pp|debt|charts|categories`, `tx.list`,
  `recurring.list`, `alert.pending`, `health`. A write verb here is rejected at
  dispatch (`forbidden`). No UID gate (the bridge uses this).
- **write.sock** — adds `provider.refresh`, `manual.upsert_provider|business|
  subscription`, `manual.update_subscription` (rename/deactivate a declared sub),
  `alert.upsert`, `recurring.confirm`. Every connection is gated
  by `getpeereid(2)`: peer UID must equal `daemon.operator_uid`, else `forbidden`.
  **Unset operator_uid fails closed** (rejects all).
- **tcp 127.0.0.1:7732** — full verb set, *no* UID gate (meaningless over TCP),
  so `bind_tcp` rejects any non-loopback bind at startup. Latent/undeployed;
  remote access is the mesh-SSH TUI on write.sock, never direct TCP. Never add a
  pf `pass in` for 7732.

## Data flow

```
providers ──scheduler──▶ balance_snapshots / transactions / price_snapshots / provider_raw
                                   │
        ┌──────────────────────────┼───────────────────────────┐
        ▼                          ▼                            ▼
  alert engine             reports engine                calendar engine
  → alert_history          → <reports_dir>/*.md          → <ics_dir>/*.ics
        │                          │                            │
        └──────────── arca-xmpp polls + pushes ─────────────────┘
                      (flips alert_history.delivered=1 via direct local SQLite write)

clients (TUI / CLI / bridge) ──RPC──▶ handler ──▶ DB read models (pp / debt / recurring / …)
```

The daemon never delivers messages itself (no `proc`/`exec`); it records to the
DB or writes files, and the bridge ships them out of band.

## Sandbox (OpenBSD)

- **pledge**: `stdio rpath wpath cpath flock fattr inet unix dns` — no
  `proc`/`exec` (the daemon spawns nothing). `flock` = SQLite WAL; `fattr` =
  socket chmod; `inet` = reqwest providers.
- **unveil**: db path + parent (rwc), log dir (rwc), the read/write socket dirs
  (rwc), `reports_dir` + `ics_dir` (rwc), `/etc/arca` (r), `/tmp` (rwc).

Applied in `apply_sandbox()` (main.rs): unveil narrows, then pledge locks.

## Database

SQLite, WAL, `foreign_keys=ON`. Numbered `.sql` files under `migrations/`,
`include_str!`-embedded and applied at open (0001–0008). Raw SQL via rusqlite
prepared statements — no ORM. **Migrations are the authoritative schema.** All
money is integer cents (`Cents`); all timestamps unix-seconds UTC, rendered AST
in clients; provider rows carry `external_id` for idempotent upsert.

## Providers

The `Provider` trait (`arca-core/src/provider.rs`) is the only contract:
`refresh_balances` / `refresh_transactions` / `refresh_usage`. `registry::load`
maps a `providers.kind` string to an impl. 12 are live (manual, plaid, mercury,
stripe, openai_usage, openrouter, scrapecreators, postmark, gold_spot, xmr_spot,
vultr, vultr_cost); xai is deferred (no public usage API — track its spend, e.g.
a SuperGrok subscription, as a `manual` recurring subscription). **Per-provider
config/secrets/status: `providers.md` (authoritative).** Adding one: the design spec
"Adding a new provider".

## Build / run

```
# OpenBSD host
doas pkg_add rust sqlite3 age
cargo build --release --workspace
```

Dev on any Unix: `cargo build --workspace` (pledge/unveil are no-ops, so it
builds and runs). Smoke the TUI without a daemon: `arca --smoke`. First-time
install + supervision (`useradd _arca`, `install` targets, `rcctl`, `rc.d/arca`
and `rc.d/arca_xmpp`) is in the design spec "Build, install, run".
