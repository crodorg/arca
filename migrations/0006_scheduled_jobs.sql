-- 0006_scheduled_jobs: bookkeeping for periodic daemon-internal jobs that
-- aren't alert rules (monthly report generation, weekly .ics digest).
--
-- One row per job `name`; `last_run_at` is the unix-seconds UTC time the job
-- last completed successfully. `last_status` is "ok" or "err:<message>".
--
-- Engines consult this table to avoid double-firing the same job in a calendar
-- period and to surface "stuck" jobs in future health output.

CREATE TABLE scheduled_jobs (
  name        TEXT PRIMARY KEY,
  last_run_at INTEGER NOT NULL DEFAULT 0,
  last_status TEXT
);
