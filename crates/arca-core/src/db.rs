//! SQLite layer. Raw SQL with prepared statements. No ORM.
//! Connection is held in a `Mutex` so `Db` is `Sync`. All ops are short and serialized.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::ids::{AccountId, BusinessId, ProviderId, TransactionId};
use crate::money::Cents;
use crate::time::now_secs;

/// Migrations, ordered. Each `(name, sql)` is applied once.
const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_init",
        include_str!("../../../migrations/0001_init.sql"),
    ),
    (
        "0002_provider_raw",
        include_str!("../../../migrations/0002_provider_raw.sql"),
    ),
    (
        "0003_accounts_external_id",
        include_str!("../../../migrations/0003_accounts_external_id.sql"),
    ),
    (
        "0004_price_snapshots",
        include_str!("../../../migrations/0004_price_snapshots.sql"),
    ),
    (
        "0005_account_tier",
        include_str!("../../../migrations/0005_account_tier.sql"),
    ),
    (
        "0006_scheduled_jobs",
        include_str!("../../../migrations/0006_scheduled_jobs.sql"),
    ),
    (
        "0007_recurring_series",
        include_str!("../../../migrations/0007_recurring_series.sql"),
    ),
    (
        "0008_recurring_label_nullable",
        include_str!("../../../migrations/0008_recurring_label_nullable.sql"),
    ),
];

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open or create the SQLite file at `path`. Runs WAL + foreign keys + migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::configure(&conn)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.apply_migrations()?;
        Ok(db)
    }

    /// In-memory DB for tests.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.apply_migrations()?;
        Ok(db)
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(())
    }

    fn apply_migrations(&self) -> Result<()> {
        let mut conn = self.lock();
        // Bootstrap _migrations.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS _migrations (
                name        TEXT PRIMARY KEY,
                applied_at  INTEGER NOT NULL
            )",
            [],
        )?;

        for (name, sql) in MIGRATIONS {
            let already: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM _migrations WHERE name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .optional()?;
            if already.is_some() {
                continue;
            }
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT OR IGNORE INTO _migrations (name, applied_at) VALUES (?1, ?2)",
                params![name, now_secs()],
            )?;
            tx.commit()?;
            tracing::info!(migration = name, "applied");
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("db mutex poisoned")
    }

    /// Borrow the connection under the lock. Use sparingly — external code that
    /// holds the guard blocks all other DB ops.
    pub fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        let conn = self.lock();
        f(&conn)
    }

    // ---- accounts ----

    pub fn upsert_account(&self, a: &Account) -> Result<AccountId> {
        let conn = self.lock();
        // Prefer (provider_id, external_id) dedup; fall back to unique name.
        let id: Option<i64> =
            if let (Some(pid), Some(ext)) = (a.provider_id, a.external_id.as_deref()) {
                conn.query_row(
                    "SELECT id FROM accounts WHERE provider_id = ?1 AND external_id = ?2",
                    params![pid.0, ext],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            } else {
                conn.query_row(
                    "SELECT id FROM accounts WHERE name = ?1",
                    params![a.name],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            };
        if let Some(id) = id {
            conn.execute(
                "UPDATE accounts SET name=?1, kind=?2, asset_class=?3, tier=?4, currency=?5,
                    provider_id=?6, business_id=?7, external_id=?8, active=?9 WHERE id=?10",
                params![
                    a.name,
                    a.kind,
                    a.asset_class,
                    a.tier,
                    a.currency,
                    a.provider_id.map(|p| p.0),
                    a.business_id.map(|b| b.0),
                    a.external_id,
                    i32::from(a.active),
                    id,
                ],
            )?;
            Ok(AccountId(id))
        } else {
            conn.execute(
                "INSERT INTO accounts (name, kind, asset_class, tier, currency, provider_id,
                    business_id, external_id, active, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    a.name,
                    a.kind,
                    a.asset_class,
                    a.tier,
                    a.currency,
                    a.provider_id.map(|p| p.0),
                    a.business_id.map(|b| b.0),
                    a.external_id,
                    i32::from(a.active),
                    now_secs(),
                ],
            )?;
            Ok(AccountId(conn.last_insert_rowid()))
        }
    }

    pub fn list_active_accounts(&self) -> Result<Vec<Account>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, asset_class, tier, currency, provider_id, business_id,
                    external_id, active
               FROM accounts WHERE active = 1 ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Account {
                    id: Some(AccountId(r.get(0)?)),
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    asset_class: r.get(3)?,
                    tier: r.get(4)?,
                    currency: r.get(5)?,
                    provider_id: r.get::<_, Option<i64>>(6)?.map(ProviderId),
                    business_id: r.get::<_, Option<i64>>(7)?.map(BusinessId),
                    external_id: r.get(8)?,
                    active: r.get::<_, i32>(9)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Find an account by (provider, external_id). Used by provider polls to map
    /// upstream account IDs to local rows.
    pub fn find_account_by_external(
        &self,
        provider_id: ProviderId,
        external_id: &str,
    ) -> Result<Option<Account>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, name, kind, asset_class, tier, currency, provider_id, business_id,
                    external_id, active
               FROM accounts
              WHERE provider_id = ?1 AND external_id = ?2",
            params![provider_id.0, external_id],
            |r| {
                Ok(Account {
                    id: Some(AccountId(r.get(0)?)),
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    asset_class: r.get(3)?,
                    tier: r.get(4)?,
                    currency: r.get(5)?,
                    provider_id: r.get::<_, Option<i64>>(6)?.map(ProviderId),
                    business_id: r.get::<_, Option<i64>>(7)?.map(BusinessId),
                    external_id: r.get(8)?,
                    active: r.get::<_, i32>(9)? != 0,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    // ---- transactions ----

    pub fn upsert_transaction(&self, t: &Transaction) -> Result<TransactionId> {
        let conn = self.lock();
        // Dedup on (account_id, external_id) when external_id is set.
        if let Some(ext) = &t.external_id {
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT id FROM transactions WHERE account_id=?1 AND external_id=?2",
                    params![t.account_id.0, ext],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                conn.execute(
                    "UPDATE transactions SET posted_at=?1, amount_cents=?2, currency=?3,
                        description=?4, category=?5, tag=?6, business_id=?7, source=?8
                     WHERE id=?9",
                    params![
                        t.posted_at,
                        t.amount_cents.0,
                        t.currency,
                        t.description,
                        t.category,
                        t.tag,
                        t.business_id.map(|b| b.0),
                        t.source,
                        id,
                    ],
                )?;
                return Ok(TransactionId(id));
            }
        }
        conn.execute(
            "INSERT INTO transactions (account_id, posted_at, amount_cents, currency,
                description, category, tag, business_id, external_id, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                t.account_id.0,
                t.posted_at,
                t.amount_cents.0,
                t.currency,
                t.description,
                t.category,
                t.tag,
                t.business_id.map(|b| b.0),
                t.external_id,
                t.source,
            ],
        )?;
        Ok(TransactionId(conn.last_insert_rowid()))
    }

    /// Delete a transaction by its provider `external_id` (e.g. a Plaid
    /// `transaction_id` reported in `/transactions/sync`'s `removed[]`). Plaid
    /// transaction ids are globally unique, so no account/source scoping is
    /// needed. Returns rows deleted (0 if we never stored it). Idempotent.
    pub fn delete_transaction_by_external(&self, external_id: &str) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM transactions WHERE external_id = ?1",
            params![external_id],
        )?;
        Ok(n)
    }

    pub fn list_transactions(
        &self,
        since: Option<i64>,
        tag: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Transaction>> {
        let mut sql = String::from(
            "SELECT id, account_id, posted_at, amount_cents, currency, description,
                    category, tag, business_id, external_id, source
               FROM transactions WHERE 1=1",
        );
        if since.is_some() {
            sql.push_str(" AND posted_at >= ?1");
        }
        if tag.is_some() {
            sql.push_str(if since.is_some() {
                " AND tag = ?2"
            } else {
                " AND tag = ?1"
            });
        }
        sql.push_str(" ORDER BY posted_at DESC LIMIT ");
        sql.push_str(&limit.to_string());

        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Transaction> {
            Ok(Transaction {
                id: Some(TransactionId(r.get(0)?)),
                account_id: AccountId(r.get(1)?),
                posted_at: r.get(2)?,
                amount_cents: Cents(r.get(3)?),
                currency: r.get(4)?,
                description: r.get(5)?,
                category: r.get(6)?,
                tag: r.get(7)?,
                business_id: r.get::<_, Option<i64>>(8)?.map(BusinessId),
                external_id: r.get(9)?,
                source: r.get(10)?,
            })
        };
        let rows: Vec<Transaction> = match (since, tag) {
            (Some(s), Some(t)) => stmt
                .query_map(params![s, t], map)?
                .collect::<std::result::Result<_, _>>()?,
            (Some(s), None) => stmt
                .query_map(params![s], map)?
                .collect::<std::result::Result<_, _>>()?,
            (None, Some(t)) => stmt
                .query_map(params![t], map)?
                .collect::<std::result::Result<_, _>>()?,
            (None, None) => stmt
                .query_map([], map)?
                .collect::<std::result::Result<_, _>>()?,
        };
        Ok(rows)
    }

    /// Sum transactions by tag in [since, until]. Cents accumulate signed.
    pub fn total_by_tag(&self, tag: &str, since: i64, until: i64) -> Result<Cents> {
        let conn = self.lock();
        let sum: i64 = conn.query_row(
            "SELECT COALESCE(SUM(amount_cents), 0) FROM transactions
               WHERE tag = ?1 AND posted_at >= ?2 AND posted_at < ?3",
            params![tag, since, until],
            |r| r.get(0),
        )?;
        Ok(Cents(sum))
    }

    /// P&L for a business in [since, until]. Income (positive) minus expenses (negative).
    pub fn business_pnl(&self, business_id: BusinessId, since: i64, until: i64) -> Result<Cents> {
        let conn = self.lock();
        let sum: i64 = conn.query_row(
            "SELECT COALESCE(SUM(amount_cents), 0) FROM transactions
               WHERE business_id = ?1 AND posted_at >= ?2 AND posted_at < ?3",
            params![business_id.0, since, until],
            |r| r.get(0),
        )?;
        Ok(Cents(sum))
    }

    // ---- balance snapshots ----

    pub fn insert_snapshot(
        &self,
        account_id: AccountId,
        amount: Cents,
        source: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO balance_snapshots (account_id, taken_at, amount_cents, source)
             VALUES (?1, ?2, ?3, ?4)",
            params![account_id.0, now_secs(), amount.0, source],
        )?;
        Ok(())
    }

    /// Insert a price reference snapshot. `commodity` is a short symbol
    /// (e.g. "XAU"). Price is in USD cents per unit (oz for metals).
    pub fn insert_price_snapshot(
        &self,
        commodity: &str,
        amount: Cents,
        source: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO price_snapshots (commodity, taken_at, price_cents, source)
             VALUES (?1, ?2, ?3, ?4)",
            params![commodity, now_secs(), amount.0, source],
        )?;
        Ok(())
    }

    /// Most recent recorded price for `commodity`, or None if none yet.
    pub fn latest_price(&self, commodity: &str) -> Result<Option<Cents>> {
        let conn = self.lock();
        let v: Option<i64> = conn
            .query_row(
                "SELECT price_cents FROM price_snapshots
                 WHERE commodity = ?1
                 ORDER BY taken_at DESC, id DESC
                 LIMIT 1",
                params![commodity],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.map(Cents))
    }

    /// Latest snapshot per account. Ties on taken_at break on highest id.
    pub fn latest_balances(&self) -> Result<Vec<(AccountId, Cents)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT account_id, amount_cents FROM (
               SELECT account_id, amount_cents,
                      ROW_NUMBER() OVER (
                        PARTITION BY account_id
                        ORDER BY taken_at DESC, id DESC
                      ) AS rn
                 FROM balance_snapshots
             ) WHERE rn = 1",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((AccountId(r.get(0)?), Cents(r.get(1)?))))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- businesses ----

    pub fn business_by_tag(&self, tag: &str) -> Result<Business> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, tag, display_name, active FROM businesses WHERE tag = ?1",
            params![tag],
            |r| {
                Ok(Business {
                    id: BusinessId(r.get(0)?),
                    tag: r.get(1)?,
                    display_name: r.get(2)?,
                    active: r.get::<_, i32>(3)? != 0,
                })
            },
        )
        .optional()?
        .ok_or_else(|| CoreError::NotFound(format!("business tag={tag}")))
    }

    /// Create or update a business by `tag` (the stable key). A NULL field
    /// preserves the stored value on update (COALESCE merge); on insert,
    /// `display_name` defaults to the tag and `active` to enabled.
    pub fn upsert_business(
        &self,
        tag: &str,
        display_name: Option<&str>,
        active: Option<bool>,
    ) -> Result<BusinessId> {
        let conn = self.lock();
        let active_int = active.map(i32::from);
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM businesses WHERE tag = ?1",
                params![tag],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            conn.execute(
                "UPDATE businesses
                    SET display_name = COALESCE(?1, display_name),
                        active       = COALESCE(?2, active)
                  WHERE id = ?3",
                params![display_name, active_int, id],
            )?;
            Ok(BusinessId(id))
        } else {
            conn.execute(
                "INSERT INTO businesses (tag, display_name, active)
                 VALUES (?1, COALESCE(?2, ?1), COALESCE(?3, 1))",
                params![tag, display_name, active_int],
            )?;
            Ok(BusinessId(conn.last_insert_rowid()))
        }
    }

    /// Create or update a subscription (recurring obligation) by `name`. Upserts
    /// on the name; on update, `business_id` is COALESCE-preserved when NULL so a
    /// re-run that omits it keeps the prior binding. Used by
    /// `manual.upsert_subscription` to declare rent/insurance/fixed bills.
    // One arg per subscription column — flatter and clearer than a builder here.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_subscription(
        &self,
        name: &str,
        provider_kind: &str,
        amount_cents: Option<Cents>,
        cadence: Option<&str>,
        next_charge_at: Option<i64>,
        business_id: Option<BusinessId>,
        active: bool,
    ) -> Result<()> {
        let conn = self.lock();
        let amt = amount_cents.map(Cents::as_i64);
        let bid = business_id.map(|b| b.0);
        let active_int = i32::from(active);
        let n = conn.execute(
            "UPDATE subscriptions
                SET provider_kind  = ?2,
                    amount_cents   = ?3,
                    cadence        = ?4,
                    next_charge_at = ?5,
                    business_id    = COALESCE(?6, business_id),
                    active         = ?7
              WHERE name = ?1",
            params![
                name,
                provider_kind,
                amt,
                cadence,
                next_charge_at,
                bid,
                active_int
            ],
        )?;
        if n == 0 {
            conn.execute(
                "INSERT INTO subscriptions
                    (name, provider_kind, amount_cents, cadence, next_charge_at, business_id, active)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![name, provider_kind, amt, cadence, next_charge_at, bid, active_int],
            )?;
        }
        Ok(())
    }

    /// Activate/deactivate a declared subscription by `name`, leaving its
    /// amount/cadence/next untouched (the narrow update `upsert_subscription`
    /// can't do — it rewrites every column). Returns the number of rows changed
    /// (0 = no such subscription). Backs the Bills schedule "Remove" action.
    pub fn set_subscription_active(&self, name: &str, active: bool) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE subscriptions SET active = ?2 WHERE name = ?1",
            params![name, i32::from(active)],
        )?;
        Ok(n)
    }

    /// Rename a declared subscription (the `name` is its identity *and* its display
    /// label — there's no separate display column). Returns rows changed (0 = no
    /// such subscription). Backs the Bills schedule "Rename" action.
    pub fn rename_subscription(&self, old_name: &str, new_name: &str) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE subscriptions SET name = ?2 WHERE name = ?1",
            params![old_name, new_name],
        )?;
        Ok(n)
    }

    // ---- providers ----

    pub fn list_providers(&self) -> Result<Vec<ProviderRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, label, config_json, secret_ref, poll_cadence,
                    last_poll_at, last_status FROM providers",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProviderRow {
                    id: ProviderId(r.get(0)?),
                    kind: r.get(1)?,
                    label: r.get(2)?,
                    config_json: r.get(3)?,
                    secret_ref: r.get(4)?,
                    poll_cadence: r.get(5)?,
                    last_poll_at: r.get(6)?,
                    last_status: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn upsert_provider(&self, p: &ProviderRow) -> Result<ProviderId> {
        let conn = self.lock();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM providers WHERE kind = ?1 AND label = ?2",
                params![p.kind, p.label],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            conn.execute(
                "UPDATE providers SET config_json=?1, secret_ref=?2,
                    poll_cadence=?3 WHERE id=?4",
                params![p.config_json, p.secret_ref, p.poll_cadence, id],
            )?;
            Ok(ProviderId(id))
        } else {
            conn.execute(
                "INSERT INTO providers (kind, label, config_json, secret_ref, poll_cadence)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![p.kind, p.label, p.config_json, p.secret_ref, p.poll_cadence],
            )?;
            Ok(ProviderId(conn.last_insert_rowid()))
        }
    }

    pub fn record_poll(&self, id: ProviderId, status: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE providers SET last_poll_at=?1, last_status=?2 WHERE id=?3",
            params![now_secs(), status, id.0],
        )?;
        Ok(())
    }

    /// Persist a `config_json` patch for a provider row (e.g. updated Plaid sync cursor).
    pub fn update_provider_config(&self, id: ProviderId, config_json: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE providers SET config_json=?1 WHERE id=?2",
            params![config_json, id.0],
        )?;
        Ok(())
    }

    // ---- alert rules / history ----

    /// Read all active alert rules. The engine evaluates each on its tick.
    pub fn list_active_alert_rules(&self) -> Result<Vec<AlertRule>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, rule_json, channel, active
               FROM alert_rules WHERE active = 1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AlertRule {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    rule_json: r.get(2)?,
                    channel: r.get(3)?,
                    active: r.get::<_, i32>(4)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Latest fire timestamp for a rule (alert_history.fired_at MAX), or None
    /// if the rule has never fired. Used by the engine for dedup-window logic.
    pub fn latest_alert_fire(&self, rule_id: i64) -> Result<Option<i64>> {
        let conn = self.lock();
        // `MAX()` returns one row whose column may be NULL when the table is
        // empty, so we read it as `Option<i64>` rather than `i64`.
        let v: Option<i64> = conn.query_row(
            "SELECT MAX(fired_at) FROM alert_history WHERE rule_id = ?1",
            params![rule_id],
            |r| r.get::<_, Option<i64>>(0),
        )?;
        Ok(v)
    }

    /// Record an alert firing. The daemon writes rows with `delivered=false`;
    /// the arca-xmpp bridge flips it to true once it has pushed the row to the
    /// operator's JID.
    pub fn insert_alert_history(
        &self,
        rule_id: i64,
        payload_json: &str,
        delivered: bool,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO alert_history (rule_id, fired_at, payload_json, delivered)
             VALUES (?1, ?2, ?3, ?4)",
            params![rule_id, now_secs(), payload_json, i32::from(delivered)],
        )?;
        Ok(())
    }

    /// Idempotent upsert of an alert rule by `name`. Used at daemon startup
    /// to seed the default `pp.band_breach` rule without requiring a UNIQUE
    /// constraint migration.
    pub fn upsert_alert_rule_by_name(
        &self,
        name: &str,
        rule_json: &str,
        channel: &str,
    ) -> Result<i64> {
        let conn = self.lock();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM alert_rules WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(id);
        }
        conn.execute(
            "INSERT INTO alert_rules (name, rule_json, channel, active)
             VALUES (?1, ?2, ?3, 1)",
            params![name, rule_json, channel],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Create or update an alert rule by `name`, setting `rule_json`, `channel`,
    /// and `active`. Unlike [`Self::upsert_alert_rule_by_name`] (insert-if-absent,
    /// used for idempotent seeding), this overwrites an existing row — it backs
    /// the `alert.upsert` RPC verb. Keyed on `name` via SELECT-then-write, so no
    /// UNIQUE index is required (mirrors `upsert_provider`).
    pub fn set_alert_rule(
        &self,
        name: &str,
        rule_json: &str,
        channel: &str,
        active: bool,
    ) -> Result<i64> {
        let conn = self.lock();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM alert_rules WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            conn.execute(
                "UPDATE alert_rules SET rule_json=?1, channel=?2, active=?3 WHERE id=?4",
                params![rule_json, channel, i32::from(active), id],
            )?;
            Ok(id)
        } else {
            conn.execute(
                "INSERT INTO alert_rules (name, rule_json, channel, active)
                 VALUES (?1, ?2, ?3, ?4)",
                params![name, rule_json, channel, i32::from(active)],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }

    /// Recent alert firings joined to their rule, newest first. `include_delivered`
    /// false (the default for `alert.pending`) returns only the undelivered queue —
    /// what the arca-xmpp bridge would push next. Backs the `alert.pending` verb.
    pub fn list_recent_alerts(
        &self,
        limit: i64,
        include_delivered: bool,
    ) -> Result<Vec<AlertHistoryRow>> {
        let conn = self.lock();
        let sql = "SELECT h.id, r.name, r.rule_json, h.fired_at, h.payload_json, h.delivered
                     FROM alert_history h
                     JOIN alert_rules r ON r.id = h.rule_id
                    WHERE (?1 = 1 OR h.delivered = 0)
                    ORDER BY h.fired_at DESC, h.id DESC
                    LIMIT ?2";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![i32::from(include_delivered), limit], |r| {
                Ok(AlertHistoryRow {
                    id: r.get(0)?,
                    rule_name: r.get(1)?,
                    rule_json: r.get(2)?,
                    fired_at: r.get(3)?,
                    payload_json: r.get(4)?,
                    delivered: r.get::<_, i32>(5)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- scheduled jobs ----

    /// Last successful completion time for a job, or `None` if never run.
    pub fn last_job_run(&self, name: &str) -> Result<Option<i64>> {
        let conn = self.lock();
        let v: Option<i64> = conn
            .query_row(
                "SELECT last_run_at FROM scheduled_jobs WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?;
        // last_run_at defaults to 0; treat 0 as "never run" so a freshly seeded
        // row doesn't masquerade as a 1970 success.
        Ok(v.filter(|&t| t > 0))
    }

    /// Mark a job as run at `at_secs` with `status`. Idempotent insert-or-update.
    pub fn record_job_run(&self, name: &str, at_secs: i64, status: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO scheduled_jobs (name, last_run_at, last_status)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET last_run_at = excluded.last_run_at,
                                             last_status = excluded.last_status",
            params![name, at_secs, status],
        )?;
        Ok(())
    }

    // ---- recurring series labels (operator annotations; see migration 0007) ----

    /// Upsert an operator label for a detected series, keyed by `match_key`
    /// (the normalized payee). Idempotent on `match_key` — re-confirming updates
    /// label/name/business/active and bumps `confirmed_at`.
    pub fn upsert_recurring_label(
        &self,
        match_key: &str,
        label: Option<&str>,
        display_name: Option<&str>,
        business_id: Option<i64>,
        active: bool,
        confirmed_at: i64,
    ) -> Result<()> {
        let conn = self.lock();
        // COALESCE on label/display_name/business_id: a NULL field in this confirm
        // PRESERVES the stored value. So `:rename` (label=NULL, name set) keeps an
        // existing sub/bill/debt label, and `:label` keeps a prior rename. `active`
        // is a direct set so a dismiss (active=0) still toggles.
        conn.execute(
            "INSERT INTO recurring_series
                 (match_key, label, display_name, business_id, active, confirmed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(match_key) DO UPDATE SET
                 label        = COALESCE(excluded.label, recurring_series.label),
                 display_name = COALESCE(excluded.display_name, recurring_series.display_name),
                 business_id  = COALESCE(excluded.business_id, recurring_series.business_id),
                 active       = excluded.active,
                 confirmed_at = excluded.confirmed_at",
            params![
                match_key,
                label,
                display_name,
                business_id,
                active,
                confirmed_at
            ],
        )?;
        Ok(())
    }

    /// All active recurring labels, for joining onto detected series. Keyed by
    /// `match_key`. Dismissed (`active = 0`) rows are excluded — a dismissed
    /// label reads as an unconfirmed series.
    pub fn recurring_labels(&self) -> Result<Vec<RecurringLabel>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT match_key, label, display_name, business_id, confirmed_at
               FROM recurring_series
              WHERE active = 1",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RecurringLabel {
                    match_key: r.get(0)?,
                    label: r.get(1)?,
                    display_name: r.get(2)?,
                    business_id: r.get(3)?,
                    confirmed_at: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- subscriptions / planned_expenses (read-only views for reports/ICS) ----

    /// All active subscriptions, ordered by name.
    pub fn list_active_subscriptions(&self) -> Result<Vec<SubscriptionRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, provider_kind, amount_cents, cadence, next_charge_at,
                    account_id, business_id, active
               FROM subscriptions
              WHERE active = 1
              ORDER BY COALESCE(next_charge_at, 9223372036854775807), name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SubscriptionRecord {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    provider_kind: r.get(2)?,
                    amount_cents: r.get::<_, Option<i64>>(3)?.map(Cents),
                    cadence: r.get(4)?,
                    next_charge_at: r.get(5)?,
                    account_id: r.get::<_, Option<i64>>(6)?.map(AccountId),
                    business_id: r.get::<_, Option<i64>>(7)?.map(BusinessId),
                    active: r.get::<_, i32>(8)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Planned (unpaid) expenses with `due_at` in `[since, until)`. Ordered by due_at asc.
    pub fn list_planned_expenses_due(&self, since: i64, until: i64) -> Result<Vec<PlannedExpense>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, due_at, amount_cents, description, account_id, business_id, status
               FROM planned_expenses
              WHERE status = 'planned' AND due_at >= ?1 AND due_at < ?2
              ORDER BY due_at",
        )?;
        let rows = stmt
            .query_map(params![since, until], |r| {
                Ok(PlannedExpense {
                    id: r.get(0)?,
                    due_at: r.get(1)?,
                    amount_cents: Cents(r.get::<_, i64>(2)?),
                    description: r.get(3)?,
                    account_id: r.get::<_, Option<i64>>(4)?.map(AccountId),
                    business_id: r.get::<_, Option<i64>>(5)?.map(BusinessId),
                    status: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Sum of expense (negative) amounts by category in `[since, until)`. Returns
    /// `(category, total_cents)` ordered by spend magnitude descending.
    /// Rows with NULL category are bucketed as `"uncategorized"`.
    pub fn expenses_by_category(
        &self,
        since: i64,
        until: i64,
        limit: i64,
    ) -> Result<Vec<(String, Cents)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT COALESCE(category, 'uncategorized') AS cat,
                    SUM(amount_cents) AS total
               FROM transactions
              WHERE posted_at >= ?1 AND posted_at < ?2 AND amount_cents < 0
              GROUP BY cat
              ORDER BY total ASC
              LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![since, until, limit], |r| {
                Ok((r.get::<_, String>(0)?, Cents(r.get::<_, i64>(1)?)))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Net-worth time series: one point per distinct snapshot instant, each the
    /// net worth as-of that instant (latest balance per active account ≤ t;
    /// debts negated, subscriptions excluded — same rule as the money snapshot).
    /// Oldest first. `since` only trims the *output*; earlier snapshots still
    /// seed each retained point's running balances. Data volume is tiny (a
    /// handful of accounts × daily snapshots), so the walk is done in Rust.
    pub fn networth_series(&self, since: Option<i64>) -> Result<Vec<(i64, Cents)>> {
        use std::collections::HashMap;
        let conn = self.lock();

        // currency = 'USD' only: a non-USD account's snapshots must not sum into
        // the USD net-worth series (no FX conversion in v1). Excluded rows fall
        // through to the `None` arm below and are skipped.
        let mut kstmt =
            conn.prepare("SELECT id, kind FROM accounts WHERE active = 1 AND currency = 'USD'")?;
        let kinds: HashMap<i64, String> = kstmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(kstmt);

        let mut sstmt = conn.prepare(
            "SELECT account_id, taken_at, amount_cents FROM balance_snapshots
             ORDER BY taken_at ASC, id ASC",
        )?;
        let snaps: Vec<(i64, i64, i64)> = sstmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(sstmt);

        let mut latest: HashMap<i64, i64> = HashMap::new();
        let mut out: Vec<(i64, Cents)> = Vec::new();
        let mut i = 0;
        while i < snaps.len() {
            let t = snaps[i].1;
            // Apply every snapshot stamped at this instant before measuring.
            while i < snaps.len() && snaps[i].1 == t {
                latest.insert(snaps[i].0, snaps[i].2);
                i += 1;
            }
            let mut net: i64 = 0;
            for (aid, amt) in &latest {
                match kinds.get(aid).map(String::as_str) {
                    Some("subscription") => {} // excluded from net worth
                    Some("debt") => net -= *amt,
                    Some(_) => net += *amt,
                    None => {} // inactive/unknown: skip
                }
            }
            out.push((t, Cents(net)));
        }
        if let Some(s) = since {
            out.retain(|(t, _)| *t >= s);
        }
        Ok(out)
    }

    /// Monthly cash flow over `[since, until)` (UTC calendar months). Returns
    /// `(YYYY-MM, income, expenses)` per month with at least one transaction,
    /// oldest first. Income is the sum of inflows (≥ 0); expenses the sum of
    /// outflows (kept negative).
    pub fn cash_flow_monthly(&self, since: i64, until: i64) -> Result<Vec<(String, Cents, Cents)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT strftime('%Y-%m', posted_at, 'unixepoch') AS ym,
                    SUM(CASE WHEN amount_cents >= 0 THEN amount_cents ELSE 0 END) AS income,
                    SUM(CASE WHEN amount_cents <  0 THEN amount_cents ELSE 0 END) AS expense
               FROM transactions
              WHERE posted_at >= ?1 AND posted_at < ?2
              GROUP BY ym
              ORDER BY ym ASC",
        )?;
        let rows = stmt
            .query_map(params![since, until], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Cents(r.get::<_, i64>(1)?),
                    Cents(r.get::<_, i64>(2)?),
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- raw provider responses ----

    pub fn insert_raw(
        &self,
        provider_id: ProviderId,
        external_id: Option<&str>,
        endpoint: &str,
        payload: &[u8],
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO provider_raw (provider_id, external_id, endpoint, fetched_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![provider_id.0, external_id, endpoint, now_secs(), payload],
        )?;
        Ok(())
    }
}

// ---- DTOs ----

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: Option<AccountId>,
    pub name: String,
    pub kind: String,
    pub asset_class: Option<String>,
    /// Capital-tier marker. `"t1"` = hold-forever backbone, `"t2"` = liquid PP
    /// operations layer (drift-tracked), `"t3"` = operating capital, `None` =
    /// uncategorized (debt, utility, subscription, business cash, etc.).
    /// See `the investment-model spec`.
    pub tier: Option<String>,
    pub currency: String,
    pub provider_id: Option<ProviderId>,
    pub business_id: Option<BusinessId>,
    pub external_id: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transaction {
    pub id: Option<TransactionId>,
    pub account_id: AccountId,
    pub posted_at: i64,
    pub amount_cents: Cents,
    pub currency: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub tag: Option<String>,
    pub business_id: Option<BusinessId>,
    pub external_id: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Business {
    pub id: BusinessId,
    pub tag: String,
    pub display_name: String,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderRow {
    pub id: ProviderId,
    pub kind: String,
    pub label: String,
    pub config_json: String,
    pub secret_ref: Option<String>,
    pub poll_cadence: String,
    pub last_poll_at: Option<i64>,
    pub last_status: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: i64,
    pub name: String,
    /// Free-form JSON. `{"kind": "..."}` selects the predicate. Supported kinds:
    /// `pp.band_breach`; `provider.stale` (optional `max_age_secs`);
    /// `balance.low` (`account`, `min_cents`).
    pub rule_json: String,
    /// Delivery channel name. Initial: `xmpp` (pushed by the arca-xmpp bridge).
    pub channel: String,
    pub active: bool,
}

/// An `alert_history` row joined to its rule. Returned by
/// [`Db::list_recent_alerts`]; the handler turns each into an `rpc::AlertRow`.
#[derive(Clone, Debug)]
pub struct AlertHistoryRow {
    pub id: i64,
    pub rule_name: String,
    /// The rule's full `rule_json` (the handler extracts `kind` + summarizes the payload).
    pub rule_json: String,
    pub fired_at: i64,
    pub payload_json: String,
    pub delivered: bool,
}

/// A persisted operator label for a recurring series (migration 0007). Only the
/// annotation lives here; the series stats stay derived in `arca_core::recurring`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecurringLabel {
    /// Normalized payee — joins to `recurring::Series.payee`.
    pub match_key: String,
    /// `sub` | `bill` | `debt` | `ignore`, or `None` for a rename-only row
    /// (display_name set without a treatment label).
    pub label: Option<String>,
    pub display_name: Option<String>,
    pub business_id: Option<i64>,
    pub confirmed_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubscriptionRecord {
    pub id: i64,
    pub name: String,
    /// `recurring` | `usage_based` | `one_time`.
    pub provider_kind: String,
    pub amount_cents: Option<Cents>,
    /// `monthly` | `yearly` | None.
    pub cadence: Option<String>,
    pub next_charge_at: Option<i64>,
    pub account_id: Option<AccountId>,
    pub business_id: Option<BusinessId>,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedExpense {
    pub id: i64,
    pub due_at: i64,
    pub amount_cents: Cents,
    pub description: String,
    pub account_id: Option<AccountId>,
    pub business_id: Option<BusinessId>,
    /// `planned` | `paid` | `skipped`.
    pub status: String,
}

impl ProviderRow {
    /// Construct a stub used when seeding a registry-built provider — no config, no secret.
    pub fn registry_stub(kind: &str, label: &str) -> Self {
        Self {
            id: ProviderId(0),
            kind: kind.into(),
            label: label.into(),
            config_json: "{}".into(),
            secret_ref: None,
            poll_cadence: "manual".into(),
            last_poll_at: None,
            last_status: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_acct(name: &str, kind: &str, ac: Option<&str>) -> Account {
        Account {
            id: None,
            name: name.into(),
            kind: kind.into(),
            asset_class: ac.map(str::to_string),
            tier: None,
            currency: "USD".into(),
            provider_id: None,
            business_id: None,
            external_id: None,
            active: true,
        }
    }

    #[test]
    fn migrations_apply_idempotently() {
        let db = Db::open_memory().unwrap();
        // Run again — should be a no-op.
        let count: i64 = db
            .with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM _migrations", [], |r| r.get(0))
                    .map_err(Into::into)
            })
            .unwrap();
        assert_eq!(count as usize, super::MIGRATIONS.len());
    }

    #[test]
    fn recurring_label_upsert_and_active_filter() {
        let db = Db::open_memory().unwrap();
        db.upsert_recurring_label("netflix", Some("sub"), Some("Netflix"), None, true, 1000)
            .unwrap();
        let rows = db.recurring_labels().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].match_key, "netflix");
        assert_eq!(rows[0].label.as_deref(), Some("sub"));
        assert_eq!(rows[0].display_name.as_deref(), Some("Netflix"));
        assert_eq!(rows[0].confirmed_at, 1000);

        // Re-confirm the same key: updates in place (no duplicate row), bumps fields.
        // A NULL field is COALESCE-preserved — so the prior display_name survives a
        // label-only re-confirm (this is what makes :rename and :label independent).
        db.upsert_recurring_label("netflix", Some("bill"), None, None, true, 2000)
            .unwrap();
        let rows = db.recurring_labels().unwrap();
        assert_eq!(rows.len(), 1, "upsert keyed on match_key, not a 2nd row");
        assert_eq!(rows[0].label.as_deref(), Some("bill"));
        assert_eq!(rows[0].display_name.as_deref(), Some("Netflix"));
        assert_eq!(rows[0].confirmed_at, 2000);

        // Soft-dismiss: row stays but drops out of the active join.
        db.upsert_recurring_label("netflix", Some("bill"), None, None, false, 3000)
            .unwrap();
        assert!(
            db.recurring_labels().unwrap().is_empty(),
            "dismissed label excluded from the active set"
        );
    }

    #[test]
    fn fk_enforced() {
        let db = Db::open_memory().unwrap();
        let res = db.with_conn(|c| {
            c.execute(
                "INSERT INTO transactions
                 (account_id, posted_at, amount_cents, currency, source)
                 VALUES (999, 0, 100, 'USD', 'manual')",
                [],
            )
            .map_err(Into::into)
        });
        assert!(res.is_err(), "FK violation expected");
    }

    #[test]
    fn account_round_trip() {
        let db = Db::open_memory().unwrap();
        let id = db
            .upsert_account(&new_acct("First Bank", "asset", Some("cash")))
            .unwrap();
        let list = db.list_active_accounts().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, Some(id));
        assert_eq!(list[0].name, "First Bank");
    }

    #[test]
    fn tx_dedup_on_external_id() {
        let db = Db::open_memory().unwrap();
        let aid = db
            .upsert_account(&new_acct("Mercury", "business", None))
            .unwrap();
        let t = Transaction {
            id: None,
            account_id: aid,
            posted_at: 1_000,
            amount_cents: Cents(50_00),
            currency: "USD".into(),
            description: Some("inv #1".into()),
            category: None,
            tag: Some("income".into()),
            business_id: None,
            external_id: Some("ext-1".into()),
            source: "manual".into(),
        };
        let id1 = db.upsert_transaction(&t).unwrap();
        let id2 = db.upsert_transaction(&t).unwrap();
        assert_eq!(id1, id2, "same external_id should update, not duplicate");
        let count: i64 = db
            .with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get(0))
                    .map_err(Into::into)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn delete_transaction_by_external_id_is_idempotent() {
        let db = Db::open_memory().unwrap();
        let aid = db
            .upsert_account(&new_acct("First Bank", "asset", None))
            .unwrap();
        let t = Transaction {
            id: None,
            account_id: aid,
            posted_at: 1_000,
            amount_cents: Cents(-15_99),
            currency: "USD".into(),
            description: Some("pending charge".into()),
            category: None,
            tag: None,
            business_id: None,
            external_id: Some("plaid-tx-1".into()),
            source: "plaid".into(),
        };
        db.upsert_transaction(&t).unwrap();
        assert_eq!(db.delete_transaction_by_external("plaid-tx-1").unwrap(), 1);
        // gone, and a second delete is a no-op (idempotent for repeated `removed`)
        assert_eq!(db.delete_transaction_by_external("plaid-tx-1").unwrap(), 0);
        assert_eq!(
            db.delete_transaction_by_external("never-stored").unwrap(),
            0
        );
        let count: i64 = db
            .with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get(0))
                    .map_err(Into::into)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn pnl_by_business() {
        let db = Db::open_memory().unwrap();
        let aid = db
            .upsert_account(&new_acct("Mercury", "business", None))
            .unwrap();
        let main = db.business_by_tag("main").unwrap();
        for (amt, ext) in [(100_00, "a"), (-30_00, "b"), (50_00, "c")] {
            let t = Transaction {
                id: None,
                account_id: aid,
                posted_at: 1_000,
                amount_cents: Cents(amt),
                currency: "USD".into(),
                description: None,
                category: None,
                tag: None,
                business_id: Some(main.id),
                external_id: Some(ext.into()),
                source: "manual".into(),
            };
            db.upsert_transaction(&t).unwrap();
        }
        let pnl = db.business_pnl(main.id, 0, i64::MAX).unwrap();
        assert_eq!(pnl, Cents(120_00));
    }

    #[test]
    fn upsert_business_inserts_then_coalesce_merges() {
        let db = Db::open_memory().unwrap();
        // Insert: display_name omitted defaults to the tag, active to enabled.
        let id = db.upsert_business("acme", None, None).unwrap();
        let b = db.business_by_tag("acme").unwrap();
        assert_eq!(b.id, id);
        assert_eq!(b.display_name, "acme");
        assert!(b.active);

        // Update display_name only — active is preserved (COALESCE), not reset.
        db.upsert_business("acme", Some("Acme LLC"), None).unwrap();
        let b = db.business_by_tag("acme").unwrap();
        assert_eq!(b.id, id, "upsert on tag, not a new row");
        assert_eq!(b.display_name, "Acme LLC");
        assert!(b.active);

        // Dismiss without touching the name — name is preserved.
        db.upsert_business("acme", None, Some(false)).unwrap();
        let b = db.business_by_tag("acme").unwrap();
        assert_eq!(b.display_name, "Acme LLC");
        assert!(!b.active);
    }

    #[test]
    fn snapshot_latest() {
        let db = Db::open_memory().unwrap();
        let aid = db
            .upsert_account(&new_acct("Brokerage", "brokerage", Some("stocks")))
            .unwrap();
        db.insert_snapshot(aid, Cents(10_000_00), "manual").unwrap();
        db.insert_snapshot(aid, Cents(11_000_00), "manual").unwrap();
        let bal = db.latest_balances().unwrap();
        assert_eq!(bal.len(), 1);
        assert_eq!(bal[0], (aid, Cents(11_000_00)));
    }

    #[test]
    fn set_alert_rule_inserts_then_updates() {
        let db = Db::open_memory().unwrap();
        let id1 = db
            .set_alert_rule("r", r#"{"kind":"provider.stale"}"#, "xmpp", true)
            .unwrap();
        // Same name → update in place, same id.
        let id2 = db
            .set_alert_rule(
                "r",
                r#"{"kind":"provider.stale","max_age_secs":600}"#,
                "xmpp",
                false,
            )
            .unwrap();
        assert_eq!(id1, id2);
        // active=false → not returned by the active-rules read.
        assert!(
            db.list_active_alert_rules()
                .unwrap()
                .iter()
                .all(|r| r.name != "r")
        );
    }

    #[test]
    fn list_recent_alerts_filters_delivered() {
        let db = Db::open_memory().unwrap();
        let rid = db
            .set_alert_rule("r", r#"{"kind":"provider.stale"}"#, "xmpp", true)
            .unwrap();
        db.insert_alert_history(rid, "[]", false).unwrap();
        db.insert_alert_history(rid, "[]", true).unwrap();

        // Default: undelivered only.
        let pending = db.list_recent_alerts(50, false).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(!pending[0].delivered);
        assert_eq!(pending[0].rule_name, "r");

        // include_delivered → both.
        assert_eq!(db.list_recent_alerts(50, true).unwrap().len(), 2);
    }

    #[test]
    fn networth_series_walks_snapshots() {
        let db = Db::open_memory().unwrap();
        let cash = db.upsert_account(&new_acct("Cash", "asset", None)).unwrap();
        let card = db.upsert_account(&new_acct("Card", "debt", None)).unwrap();
        let sub = db
            .upsert_account(&new_acct("Anthropic", "subscription", None))
            .unwrap();
        // t=1000: cash 100, card 30 (debt → subtract), sub 999 (excluded) → 70.
        // t=2000: cash bumps to 150, card unchanged → 120.
        db.with_conn(|c| {
            for (aid, t, amt) in [
                (cash.0, 1000_i64, 100_00_i64),
                (card.0, 1000, 30_00),
                (sub.0, 1000, 999_00),
                (cash.0, 2000, 150_00),
            ] {
                c.execute(
                    "INSERT INTO balance_snapshots (account_id, taken_at, amount_cents, source)
                     VALUES (?1, ?2, ?3, 'test')",
                    params![aid, t, amt],
                )?;
            }
            Ok(())
        })
        .unwrap();

        let series = db.networth_series(None).unwrap();
        assert_eq!(series, vec![(1000, Cents(70_00)), (2000, Cents(120_00))]);

        // `since` trims output but earlier snapshots still seed the running balance.
        let trimmed = db.networth_series(Some(1500)).unwrap();
        assert_eq!(trimmed, vec![(2000, Cents(120_00))]);
    }

    #[test]
    fn cash_flow_monthly_buckets_by_month() {
        let db = Db::open_memory().unwrap();
        let aid = db
            .upsert_account(&new_acct("Checking", "asset", None))
            .unwrap();
        let jan = 1_768_435_200; // 2026-01-15 UTC
        let feb = 1_770_681_600; // 2026-02-10 UTC
        for (amt, at, ext) in [(500_00, jan, "a"), (-200_00, jan, "b"), (-150_00, feb, "c")] {
            db.upsert_transaction(&Transaction {
                id: None,
                account_id: aid,
                posted_at: at,
                amount_cents: Cents(amt),
                currency: "USD".into(),
                description: None,
                category: None,
                tag: None,
                business_id: None,
                external_id: Some(ext.into()),
                source: "manual".into(),
            })
            .unwrap();
        }
        let flow = db.cash_flow_monthly(0, i64::MAX).unwrap();
        assert_eq!(
            flow,
            vec![
                ("2026-01".to_string(), Cents(500_00), Cents(-200_00)),
                ("2026-02".to_string(), Cents(0), Cents(-150_00)),
            ]
        );
    }
}
