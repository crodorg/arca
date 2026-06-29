-- Price reference snapshots — for commodities and any other quote source that
-- is not tied to a holding account. Used by the gold_spot provider (and future
-- providers like silver, crypto spot, etc) to record market reference prices
-- without polluting balance_snapshots, which is per-account.
--
-- The PP allocation logic does not yet consume this table — accounts in
-- asset_class='gold' continue to track their USD value in balance_snapshots
-- (operator-maintained). This table feeds the monthly Markdown report and the
-- future PP-drift-with-live-spot view.

CREATE TABLE price_snapshots (
  id         INTEGER PRIMARY KEY,
  commodity  TEXT NOT NULL,           -- 'XAU' (gold), future: 'XAG', 'BTC', ...
  taken_at   INTEGER NOT NULL,        -- unix seconds UTC
  price_cents INTEGER NOT NULL,       -- USD per unit (oz for XAU/XAG, coin for BTC)
  source     TEXT NOT NULL            -- provider kind that wrote this row
);
CREATE INDEX idx_price_commodity_time ON price_snapshots(commodity, taken_at);
