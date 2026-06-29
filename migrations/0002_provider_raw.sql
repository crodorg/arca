-- Raw provider responses for audit + bug-recovery. Append-only; prune later if
-- disk pressure shows up. See docs/providers.md.

CREATE TABLE provider_raw (
  id          INTEGER PRIMARY KEY,
  provider_id INTEGER NOT NULL REFERENCES providers(id),
  external_id TEXT,            -- e.g. Plaid item_id, Mercury account_id; NULL for raw payloads
  endpoint    TEXT NOT NULL,   -- e.g. 'plaid:/transactions/sync'
  fetched_at  INTEGER NOT NULL,
  payload     BLOB NOT NULL    -- raw JSON bytes
);
CREATE INDEX idx_raw_provider_time ON provider_raw(provider_id, fetched_at DESC);
CREATE INDEX idx_raw_ext           ON provider_raw(provider_id, external_id);
