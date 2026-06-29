-- arca v1 schema. All amounts in integer cents. All timestamps unix seconds UTC.
-- See the design spec for conventions and rationale.

PRAGMA foreign_keys = ON;

-- _migrations is created by the migration runner (arca_core::db::apply_migrations).

CREATE TABLE businesses (
  id           INTEGER PRIMARY KEY,
  tag          TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  active       INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE providers (
  id           INTEGER PRIMARY KEY,
  kind         TEXT NOT NULL,
  label        TEXT NOT NULL,
  config_json  TEXT NOT NULL,
  secret_ref   TEXT,
  poll_cadence TEXT NOT NULL,
  last_poll_at INTEGER,
  last_status  TEXT
);

CREATE TABLE accounts (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL,
  kind        TEXT NOT NULL,           -- asset|debt|brokerage|business|utility|subscription
  asset_class TEXT,                    -- cash|stocks|bonds|gold|crypto|other
  currency    TEXT NOT NULL DEFAULT 'USD',
  provider_id INTEGER REFERENCES providers(id),
  business_id INTEGER REFERENCES businesses(id),
  active      INTEGER NOT NULL DEFAULT 1,
  created_at  INTEGER NOT NULL
);

CREATE TABLE transactions (
  id           INTEGER PRIMARY KEY,
  account_id   INTEGER NOT NULL REFERENCES accounts(id),
  posted_at    INTEGER NOT NULL,
  amount_cents INTEGER NOT NULL,
  currency     TEXT NOT NULL DEFAULT 'USD',
  description  TEXT,
  category     TEXT,
  tag          TEXT,                   -- debt|income|investment_gain|business|other
  business_id  INTEGER REFERENCES businesses(id),
  external_id  TEXT,
  source       TEXT NOT NULL,
  UNIQUE(account_id, external_id)
);
CREATE INDEX idx_tx_posted   ON transactions(posted_at);
CREATE INDEX idx_tx_business ON transactions(business_id);

CREATE TABLE balance_snapshots (
  id           INTEGER PRIMARY KEY,
  account_id   INTEGER NOT NULL REFERENCES accounts(id),
  taken_at     INTEGER NOT NULL,
  amount_cents INTEGER NOT NULL,
  source       TEXT NOT NULL
);
CREATE INDEX idx_snap_account_time ON balance_snapshots(account_id, taken_at);

CREATE TABLE subscriptions (
  id             INTEGER PRIMARY KEY,
  name           TEXT NOT NULL,
  provider_kind  TEXT NOT NULL,        -- recurring|usage_based|one_time
  amount_cents   INTEGER,
  cadence        TEXT,                 -- monthly|yearly|NULL
  next_charge_at INTEGER,
  account_id     INTEGER REFERENCES accounts(id),
  business_id    INTEGER REFERENCES businesses(id),
  active         INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE planned_expenses (
  id           INTEGER PRIMARY KEY,
  due_at       INTEGER NOT NULL,
  amount_cents INTEGER NOT NULL,
  description  TEXT NOT NULL,
  account_id   INTEGER REFERENCES accounts(id),
  business_id  INTEGER REFERENCES businesses(id),
  status       TEXT NOT NULL DEFAULT 'planned'  -- planned|paid|skipped
);

CREATE TABLE alert_rules (
  id        INTEGER PRIMARY KEY,
  name      TEXT NOT NULL,
  rule_json TEXT NOT NULL,
  channel   TEXT NOT NULL DEFAULT 'email',
  active    INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE alert_history (
  id           INTEGER PRIMARY KEY,
  rule_id      INTEGER REFERENCES alert_rules(id),
  fired_at     INTEGER NOT NULL,
  payload_json TEXT NOT NULL,
  delivered    INTEGER NOT NULL DEFAULT 0
);

INSERT INTO businesses (tag, display_name, active) VALUES ('main', 'Main Business', 1);
