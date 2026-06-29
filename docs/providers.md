# arca — providers

A provider is a struct implementing `arca_core::provider::Provider`. Add one by:

1. Writing the impl in `crates/arca-daemon/src/providers/<name>.rs`.
2. Pushing it into the registry in `crates/arca-daemon/src/providers/registry.rs`.
3. Documenting credentials shape here.
4. Adding a row to `providers` (via `manual.upsert_provider`) at first start —
   or letting the daemon seed it from the registry on boot, which is what
   Phase 1 does.

The trait is the only contract. Do **not** add code paths in `arca-core` that
know about specific provider kinds.

## Phase 1: `manual`

No remote source. Data flows in via the `manual.*` RPC verbs:

- `manual.upsert_account { name, account_kind, asset_class, currency?, business_tag? }`
- `manual.insert_transaction { account_name, posted_at, amount, ... }`
- `manual.snapshot { account_name, amount }`

## Phase 2 decisions (resolved)

- **Raw response storage**: enabled. Every provider call writes its raw JSON
  body to `provider_raw` (migration 0002). Index on `(provider_id, fetched_at DESC)`.
  Pruning policy is deferred — open question for Phase 6.
- **age decryption**: pure Rust `age` crate, no shell-out, no `proc exec` use
  for secrets at startup. (The daemon spawns nothing — email was removed
  2026-05-24, so `proc exec` is no longer in the pledge promise at all.)
- **Plaid Link**: operator obtains the `access_token` out-of-band, pastes it
  into `/etc/arca/secrets.age` as `plaid_<label>_access_token`, then creates the
  `providers` row with `arca provider-set` (no hand-written SQL). See
  "Wiring Plaid (sandbox)" below.
- **Plaid env first integration**: sandbox first (`plaid_env = "sandbox"` in
  `config_json`), production after the integration shape is proven.

## Wiring Plaid (sandbox) — runbook

Sandbox needs no bank approval and serves synthetic recurring transactions, so it
proves the whole pipeline (refresh → accounts → tx → `recurring.list`) before any
real institution. arca only *uses* the token; minting it is two out-of-band API
calls you run from a laptop.

1. **Credentials** (laptop): sign up at plaid.com → Team Settings → Keys. Note
   `client_id` and the **Sandbox** secret.

2. **Mint a sandbox `access_token`** (laptop, two POSTs — no browser/Link UI):
   ```sh
   CID=...; SEC=...        # your client_id / sandbox secret
   PUB=$(curl -s https://sandbox.plaid.com/sandbox/public_token/create \
     -H 'Content-Type: application/json' \
     -d "{\"client_id\":\"$CID\",\"secret\":\"$SEC\",\
          \"institution_id\":\"ins_109508\",\"initial_products\":[\"transactions\"]}" \
     | sed 's/.*"public_token":"\([^"]*\)".*/\1/')
   curl -s https://sandbox.plaid.com/item/public_token/exchange \
     -H 'Content-Type: application/json' \
     -d "{\"client_id\":\"$CID\",\"secret\":\"$SEC\",\"public_token\":\"$PUB\"}"
   # -> {"access_token":"access-sandbox-...","item_id":"..."}
   ```
   (`ins_109508` = "First Platypus Bank", a standard sandbox institution.)

3. **Secrets** — add to `/etc/arca/secrets.age` (TOML, then re-`age`-encrypt):
   ```toml
   plaid_client_id        = "..."
   plaid_sandbox_secret   = "..."
   plaid_sandbox_access_token = "access-sandbox-..."
   ```

4. **Provider row** (router, write socket → operator UID):
   ```sh
   arca provider-set --kind plaid --label "Plaid Sandbox" \
     --config '{"plaid_env":"sandbox","institution_name":"First Platypus"}' \
     --secret-ref plaid_sandbox_access_token --cadence daily
   ```

5. **Refresh + verify**:
   ```sh
   arca refresh --provider plaid        # pulls balances + /transactions/sync
   arca recurring                       # synthetic subs/bills should appear
   arca health                          # plaid last_status = ok
   ```

Production differs only in steps 1–2: a real `production` secret (needs Plaid
approval) and **hosted Link** instead of `/sandbox/public_token/create` to get a
real `access_token`. Steps 3–5 are identical with `plaid_env = "production"` and
`plaid_production_secret`.

**History depth is institution-capped (2026-05-25).** The default
`transactions.days_requested` is 90; raising it to 730 via an update-mode Hosted
Link only helps if the *institution* exposes more. Several major US
institutions each return only ~90 days regardless (verified:
`/transactions/get` over a 730-day window returns exactly the 90-day set, no
error). The official Plaid CLI's `link` has no `days_requested` flag, so the bump
requires raw `/link/token/create` anyway. Treat the never-pruned `transactions`
table as the long-history record: it grows forward from each scheduled sync, and
quarterly/yearly recurring series mature only as that forward history accumulates.

**Balance semantics (2026-06-21).** Depository accounts (checking/savings)
snapshot Plaid's **`available`** balance — spendable, net of pending holds —
which matches the bank UI's headline number and keeps net worth from overstating
cash by holds that haven't posted; it falls back to `current` when Plaid omits
`available`. Credit/loan/investment accounts use **`current`** (amount owed /
position value; `available` on a credit line is remaining credit, not a balance).
The choice lives in `pick_balance()` in `plaid.rs`. Example: a checking account can
report a higher `current` than `available` when debit holds are still pending —
recording `available` keeps net worth aligned with the bank's spendable figure.

## Coming providers (priorities)

| Phase | Kind              | Status                                                     |
|-------|-------------------|------------------------------------------------------------|
| 2     | `plaid`           | shipped — sandbox+production, `/transactions/sync`         |
| 2     | `mercury`         | shipped — Bearer, per-business                             |
| 2     | `stripe`          | shipped — restricted key, per-business                     |
| 3     | `openrouter`      | shipped — `/api/v1/credits`, snapshot in USD               |
| 3     | `scrapecreators`  | shipped — `x-api-key`, snapshot in CREDITS                 |
| 3     | `postmark`        | shipped — account token + per-server stats, in MESSAGES    |
| 3     | `xai`             | deferred — no public usage API as of 2026-05-22            |
| 3     | `openai_usage`    | shipped — `/v1/organization/costs`, MTD USD (Admin key)    |
| 4     | `gold_spot`       | shipped — XAU spot → `price_snapshots` (reference only)     |
| 4     | `xmr_spot`        | shipped 2026-05-29 — XMR/USD (Kraken) × quantity → balance  |
| infra | `vultr`           | shipped — MTD outbound GB; pairs with `bandwidth.high` alert |
| infra | `vultr_cost`      | shipped 2026-06-01 — account-wide monthly USD run-rate (fleet × plan cost) |
| 5     | ~~`aaa_scraper`~~     | **cancelled 2026-05-25** — see Recurring detection     |
| 5     | ~~`liberty_scraper`~~ | **cancelled** — derived from transactions instead      |
| 5     | ~~`luma_scraper`~~    | **cancelled** — derived from transactions instead      |

Phase-5 portal scraping is replaced by `arca_core::recurring`: subscriptions /
bills / recurring debt payments are derived from the transaction stream
(`recurring.list`). HTTP-only scraping of OTP-gated, JS-walled portals is
infeasible for an unattended daemon and unnecessary.

## OpenAI: shipped usage auto-pull

**`openai_usage`** pulls MTD spend from `GET /v1/organization/costs`
(`Authorization: Bearer sk-admin-...`; `start_time` = first of the current UTC
month unix, `bucket_width=1d`). Here `amount.value` is a **number in dollars**
(`0.06` = 6¢), so it is summed and ×100. Secret: `openai_<label>_admin_key`.
Snapshots to `OpenAI API — <label>` (`currency = USD`).

It follows pagination (`has_more` / `next_page`) up to a 40-page cap and writes
each page's raw body to `provider_raw`.

(An `anthropic_usage` provider was removed 2026-05-27: its cost_report endpoint
needs an org-only Admin key, unavailable on the operator's individual account.
Track Anthropic spend via a synthetic `manual` account instead.)

**xAI** still does not expose a usage API (`docs.x.ai` as of 2026-05-22 lists
only chat/responses endpoints). Fallback: `manual.snapshot` against an `xAI API`
synthetic account. The `xai` module is a placeholder pending an API.

## Vultr: bandwidth monitoring

**`vultr`** watches one instance's month-to-date outbound transfer so a VPS
nearing its quota alerts before overage (built for the web1 box, which
self-hosts its map tiles and so has real egress). It writes no money — just a
usage gauge that the `bandwidth.high` alert reads.

- Endpoints (Bearer `Authorization`): `GET /v2/instances/{id}` for
  `allowed_bandwidth` (the monthly quota, GB) and `GET /v2/instances/{id}/bandwidth`
  for the date-keyed daily byte counts. Outbound bytes for the current calendar
  month are summed and divided by 1e9 (Vultr's decimal GB).
- Snapshot: month-to-date egress in **GB** to a `subscription` account
  `Vultr — <label> (egress GB)` (`currency = "GB"`, `external_id = "mtd_egress"`).
  The quota isn't snapshotted; it's folded into the refresh message
  (`MTD egress: 1612/2000 GB (81%)`).
- Secret: `vultr_<label>_api_key`. Config: `{"instance_id": "<uuid>"}`.

Wiring (router, write socket):

```sh
# 1. secret in /etc/arca/secrets.age (TOML, re-age-encrypt):
#    vultr_web1_api_key = "..."
# 2. provider row:
arca provider-set --kind vultr --label web1 \
  --config '{"instance_id":"00000000-0000-0000-0000-000000000000"}' \
  --secret-ref vultr_web1_api_key --cadence daily
# 3. alert rule — fire above 80% of the plan quota (e.g. 1600 of a 2000 GB plan):
arca alert-set --name web1.bandwidth \
  --rule '{"kind":"bandwidth.high","account":"Vultr — web1 (egress GB)","max_gb":1600}'
arca refresh --provider vultr     # verify; account + snapshot appear
```

The threshold is absolute GB (operator-set), mirroring `balance.low`. Resize the
plan → update `max_gb`. The fired alert rides the normal `alert_history` →
`arca-xmpp` push to your JID.

> **Key scope:** Vultr personal access tokens are **account-wide and
> full-access** — there is no read-only scope. arca only ever issues GETs, but
> the token it holds *could* mutate the account if leaked. Mitigate with Vultr's
> **API IP allow-list** (Account → API): restrict the token to the router's
> egress IP so it's unusable from anywhere else. The token still lives in
> `secrets.age` like every other credential.

## Vultr: monthly cost (run-rate)

**`vultr_cost`** answers "how much am I spending on VPSs?" **account-wide** — so
adding another instance is tracked automatically with no config change. It's
distinct from `vultr` (which is per-instance egress GB for `bandwidth.high`); the
two share only the Bearer key.

The model is lifted from an internal Vultr overlay (`a sibling project overlay`),
but transport is `reqwest`, not a shelled-out `curl` — the daemon pledges without
`proc`/`exec`.

- Endpoints (Bearer `Authorization`): `GET /v2/instances?per_page=500` (the fleet,
  each instance carries a `plan` id) and `GET /v2/plans?per_page=500` (plan id →
  `monthly_cost`, USD/month). Joined on the plan id; each live instance's plan
  cost is summed → total monthly **run-rate**.
- This is a **forward run-rate** (what the current fleet costs per month), not
  month-to-date actuals — the stabler, more useful figure for burn, and what helm
  already computed. An instance on a plan absent from the catalog **errors**
  (→ `provider.stale`) rather than silently undercounting the bill.
- Snapshot: total monthly cost in **USD** to a `subscription` account (default
  `Vultr VPS hosting`, `external_id = "monthly_cost"`). `subscription` kind keeps
  it **out of net worth** (it's a recurring cost, not an asset) while surfacing it
  in the money snapshot's subscriptions section + the monthly report's
  API/subscription table — same as `openrouter`/`postmark`. Refresh message:
  `4 instances · $48.00/mo run-rate`.
- Secret: `vultr_<label>_api_key`. Config: `{}` (or `{"account_name":"..."}` to
  rename the local account).

Wiring (router, write socket):

```sh
# 1. secret in /etc/arca/secrets.age (TOML, re-age-encrypt):
#    vultr_main_api_key = "..."
# 2. provider row (account-wide — no instance_id):
arca provider-set --kind vultr_cost --label main \
  --config '{}' --secret-ref vultr_main_api_key --cadence daily
arca refresh --provider vultr_cost   # verify; account + snapshot appear
```

The same account-wide key works for both `vultr` and `vultr_cost`; the key-scope
warning above (account-wide, no read-only scope — IP-allow-list it) applies here
too.

## XMR holding: spot-valued in USD

**`xmr_spot`** keeps a Monero holding's USD value live in net worth. `gold_spot`
only records a *reference price* (ounces held aren't tracked, so the gold
account's USD value is operator-maintained); `xmr_spot` knows the coin count and
*values the holding*.

- Source: `GET https://api.kraken.com/0/public/Ticker?pair=XMRUSD` (keyless). The
  single `result` entry's `c[0]` is the last trade price (USD/coin). A non-empty
  `error` array, a non-numeric price, or `price <= 0` fails the refresh (honest
  failure — no stale/zero balance).
- Writes: a USD `balance_snapshot` of `quantity × price` to the bound account
  (default `xmr_wallet`), upserted **by name** so an existing manual `xmr_wallet`
  row is reused (no duplicate) and stamped `kind=asset`, `asset_class=xmr`,
  `tier=t1` (hold-forever backbone). Also records an `XMR` `price_snapshot`
  (USD/coin reference series, parallel to gold's `XAU`).
- Config: `{"quantity": 3.5, "account_name": "xmr_wallet"}` — `quantity` required
  (a missing/negative/non-finite value fails the build, so the row is skipped and
  logged, never valued at zero). No secret (`secret_ref` unused).

Wiring (router, write socket):

```sh
# Keyless — no secret needed. Just the provider row:
arca provider-set --kind xmr_spot --label Monero \
  --config '{"quantity":3.5}' --cadence daily
# Registry loads providers at startup → restart to pick up the new row:
doas rcctl restart arca
arca refresh --provider xmr_spot   # verify; xmr_wallet balance + XMR price appear
```

Update the held amount by re-running `provider-set` with a new `quantity` (upserts
on kind+label) and restarting. The valuation refreshes each poll at the daily
cadence.

## Subscription account units

Three provider kinds write to `kind = subscription` accounts with **non-USD**
units, signalled by the account's `currency` field:

| `currency` | Meaning                | Providers          |
|------------|------------------------|--------------------|
| `USD`      | cumulative / MTD spend | `openrouter`, `openai_usage` |
| `CREDITS`  | credits remaining      | `scrapecreators`   |
| `MESSAGES` | MTD outbound messages  | `postmark`         |
| `GB`       | MTD outbound transfer  | `vultr`            |

Subscription accounts are **excluded** from `net_worth` and `by_kind`
aggregates in the `money` snapshot — they appear in a separate
`subscriptions` array in the response. The TUI renders them in their own
"API & subscriptions" section with unit-aware formatting.
