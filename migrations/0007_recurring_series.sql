-- Persisted operator labels for detected recurring series.
--
-- Detection stays DERIVED: arca_core::recurring recomputes the series (cadence,
-- amounts, predicted_next) from the transaction stream on every recurring.list
-- call, never stored. This table holds ONLY operator intent — "this repeating
-- payee is a subscription / bill / debt" — keyed by the detector's normalized
-- payee (`recurring::normalize_payee`, == Series.payee). recurring.list LEFT
-- JOINs on match_key to attach the label; the stats are never persisted, so
-- they can never go stale.
CREATE TABLE recurring_series (
  id           INTEGER PRIMARY KEY,
  match_key    TEXT NOT NULL UNIQUE,            -- normalized payee (join key)
  label        TEXT NOT NULL,                   -- sub | bill | debt
  display_name TEXT,                            -- optional operator-friendly name
  business_id  INTEGER REFERENCES businesses(id),
  active       BOOLEAN NOT NULL DEFAULT 1,      -- 0 = label soft-dismissed
  confirmed_at INTEGER NOT NULL                 -- unix seconds, last confirm
);
