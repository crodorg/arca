-- Make recurring_series.label nullable so a row can carry an operator-friendly
-- display_name (a rename) INDEPENDENT of a sub/bill/debt label. Previously label
-- was NOT NULL, which forced "label it first" before renaming — wrong: the
-- operator wants to rename any detected series, labeled or not.
--
-- SQLite can't drop a NOT NULL constraint in place, so rebuild the table. Nothing
-- references recurring_series.id (the only FK is the outgoing business_id), so the
-- rename/create/copy/drop is safe inside the migration transaction.
ALTER TABLE recurring_series RENAME TO recurring_series_old;

CREATE TABLE recurring_series (
  id           INTEGER PRIMARY KEY,
  match_key    TEXT NOT NULL UNIQUE,            -- normalized payee (join key)
  label        TEXT,                            -- sub|bill|debt|ignore | NULL (rename-only)
  display_name TEXT,                            -- optional operator-friendly name
  business_id  INTEGER REFERENCES businesses(id),
  active       BOOLEAN NOT NULL DEFAULT 1,      -- 0 = label/row soft-dismissed
  confirmed_at INTEGER NOT NULL                 -- unix seconds, last confirm
);

INSERT INTO recurring_series
       (id, match_key, label, display_name, business_id, active, confirmed_at)
SELECT  id, match_key, label, display_name, business_id, active, confirmed_at
  FROM recurring_series_old;

DROP TABLE recurring_series_old;
