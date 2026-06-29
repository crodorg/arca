-- Add tier column to accounts to model the operator's three-tier capital
-- structure (see the investment-model spec):
--   t1 = hold-forever antifragile backbone (gold/silver/xmr/land/sfr) — no rebalance
--   t2 = liquid PP-style operations layer (equity / long_treasuries / cash) — bands
--   t3 = operating capital (emergency fund, SaaS working capital)
--   NULL = uncategorized (debts, utilities, subscriptions, business cash, etc.)
--
-- The PP drift engine in arca-core::pp reads tier='t2' rows and groups by
-- asset_class. T1 rows are summarized info-only and never trigger drift alerts.
ALTER TABLE accounts ADD COLUMN tier TEXT;
