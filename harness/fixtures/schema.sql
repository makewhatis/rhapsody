CREATE TABLE runs (
  id               INTEGER PRIMARY KEY,
  issue_id         TEXT    NOT NULL DEFAULT '',
  issue_identifier TEXT    NOT NULL DEFAULT '',
  title            TEXT    NOT NULL DEFAULT '',
  attempt          INTEGER NOT NULL DEFAULT 0,
  session_uuid     TEXT    NOT NULL DEFAULT '',
  branch           TEXT    NOT NULL DEFAULT '',
  started_at       TEXT    NOT NULL DEFAULT '',
  ended_at         TEXT    NOT NULL DEFAULT '',
  outcome          TEXT    NOT NULL DEFAULT '',
  turns            INTEGER NOT NULL DEFAULT 0,
  input_tokens     INTEGER NOT NULL DEFAULT 0,
  output_tokens    INTEGER NOT NULL DEFAULT 0,
  total_tokens     INTEGER NOT NULL DEFAULT 0,
  error            TEXT    NOT NULL DEFAULT '',
  transcript_path  TEXT    NOT NULL DEFAULT ''
, project_slug TEXT NOT NULL DEFAULT '', repo         TEXT NOT NULL DEFAULT '', usage_estimated INTEGER NOT NULL DEFAULT 0, team_id TEXT NOT NULL DEFAULT '');
CREATE TABLE events (
  id      INTEGER PRIMARY KEY,
  run_id  INTEGER NOT NULL REFERENCES runs(id),
  seq     INTEGER NOT NULL DEFAULT 0,
  at      TEXT    NOT NULL DEFAULT '',
  kind    TEXT    NOT NULL DEFAULT '',
  tool    TEXT    NOT NULL DEFAULT '',
  text    TEXT    NOT NULL DEFAULT ''
);
CREATE TABLE retry_queue (
  issue_id   TEXT PRIMARY KEY,
  identifier TEXT    NOT NULL DEFAULT '',
  attempt    INTEGER NOT NULL DEFAULT 0,
  due_at_ms  INTEGER NOT NULL DEFAULT 0,
  error      TEXT    NOT NULL DEFAULT ''
, project_slug TEXT NOT NULL DEFAULT '');
CREATE TABLE claims (
  issue_id   TEXT PRIMARY KEY,
  state      TEXT NOT NULL DEFAULT '',
  claimed_at TEXT NOT NULL DEFAULT ''
, project_slug TEXT NOT NULL DEFAULT '');
CREATE TABLE totals (
  id              INTEGER PRIMARY KEY CHECK (id = 1),
  input_tokens    INTEGER NOT NULL DEFAULT 0,
  output_tokens   INTEGER NOT NULL DEFAULT 0,
  total_tokens    INTEGER NOT NULL DEFAULT 0,
  seconds_running INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_runs_identifier_started ON runs(issue_identifier, started_at);
CREATE INDEX idx_runs_outcome            ON runs(outcome);
CREATE INDEX idx_events_run_seq          ON events(run_id, seq);
CREATE INDEX idx_events_text             ON events(text);
CREATE INDEX idx_runs_project ON runs(project_slug);
CREATE TABLE run_messages (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id        INTEGER NOT NULL,
  body          TEXT    NOT NULL,
  created_at_ms INTEGER NOT NULL,
  status        TEXT    NOT NULL DEFAULT 'sent',
  delivered_turn INTEGER
);
CREATE INDEX idx_run_messages_run ON run_messages(run_id, id);
