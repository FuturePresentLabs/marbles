CREATE TABLE IF NOT EXISTS marbles_project (
  company_id TEXT NOT NULL,
  slug TEXT NOT NULL,
  root TEXT NOT NULL,
  prefix TEXT NOT NULL,
  policy_json TEXT NOT NULL DEFAULT '{}',
  created_at BIGINT NOT NULL,
  PRIMARY KEY (company_id, slug),
  UNIQUE (company_id, root)
);
CREATE TABLE IF NOT EXISTS marbles_issue (
  company_id TEXT NOT NULL,
  id TEXT NOT NULL,
  project TEXT NOT NULL,
  title TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL CHECK(status IN ('open','in_progress','review','blocked','deferred','closed')),
  issue_type TEXT NOT NULL DEFAULT 'task',
  priority BIGINT NOT NULL DEFAULT 2,
  parent TEXT,
  labels_json TEXT NOT NULL DEFAULT '[]',
  assignee TEXT,
  actor_kind TEXT,
  claimed_at BIGINT,
  expires_at BIGINT,
  available_at BIGINT,
  created_by TEXT NOT NULL DEFAULT '',
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  closed_at BIGINT,
  close_reason TEXT,
  evidence_json TEXT NOT NULL DEFAULT '[]',
  metadata_json TEXT NOT NULL DEFAULT '{}',
  external_ref TEXT,
  PRIMARY KEY (company_id, id),
  FOREIGN KEY (company_id, project) REFERENCES marbles_project(company_id, slug)
);
CREATE INDEX IF NOT EXISTS marbles_issue_open ON marbles_issue(company_id, status, project);
CREATE INDEX IF NOT EXISTS marbles_issue_expiry ON marbles_issue(company_id, expires_at);
CREATE TABLE IF NOT EXISTS marbles_dep (
  company_id TEXT NOT NULL,
  issue_id TEXT NOT NULL,
  depends_on TEXT NOT NULL,
  PRIMARY KEY(company_id, issue_id, depends_on),
  FOREIGN KEY(company_id, issue_id) REFERENCES marbles_issue(company_id, id),
  FOREIGN KEY(company_id, depends_on) REFERENCES marbles_issue(company_id, id)
);
CREATE TABLE IF NOT EXISTS marbles_history (
  company_id TEXT NOT NULL,
  seq BIGINT NOT NULL,
  issue_id TEXT NOT NULL,
  ts BIGINT NOT NULL,
  actor TEXT NOT NULL,
  event TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '',
  PRIMARY KEY(company_id, seq),
  FOREIGN KEY(company_id, issue_id) REFERENCES marbles_issue(company_id, id)
);
CREATE TABLE IF NOT EXISTS marbles_outbound_event (
  company_id TEXT NOT NULL,
  seq BIGINT NOT NULL,
  issue_id TEXT NOT NULL,
  project TEXT NOT NULL,
  occurred_at BIGINT NOT NULL,
  event TEXT NOT NULL,
  delivered_at BIGINT,
  attempts BIGINT NOT NULL DEFAULT 0,
  next_attempt_at BIGINT NOT NULL,
  last_error TEXT,
  PRIMARY KEY(company_id, seq),
  FOREIGN KEY(company_id, issue_id) REFERENCES marbles_issue(company_id, id)
);
CREATE INDEX IF NOT EXISTS marbles_outbound_event_pending
  ON marbles_outbound_event(company_id, delivered_at, next_attempt_at, seq);
