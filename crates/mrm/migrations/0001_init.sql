-- Full schema. The core tables were owned by the removed Python side
-- (scraper/db.py before 4006037); restored here with Postgres types.
-- Migrations are ADDITIVE ONLY: older binaries run with ignore_missing.

CREATE TABLE manhwa (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    title           TEXT NOT NULL,
    cover_url       TEXT,
    source          TEXT NOT NULL,
    source_url      TEXT NOT NULL UNIQUE,
    pub_status      TEXT NOT NULL DEFAULT 'ongoing'
                    CHECK (pub_status IN ('ongoing','hiatus','completed')),
    status          TEXT NOT NULL DEFAULT 'looked_into'
                    CHECK (status IN ('looked_into','reading','up_to_date','paused','completed','dropped')),
    status_override BOOLEAN NOT NULL DEFAULT FALSE,
    description     TEXT,
    added_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE chapter (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    manhwa_id   BIGINT NOT NULL REFERENCES manhwa(id) ON DELETE CASCADE,
    number      DOUBLE PRECISION NOT NULL,
    title       TEXT,
    url         TEXT NOT NULL UNIQUE,
    released_at TEXT,
    -- NULL = no notifying poller has announced this chapter yet (see claim_unnotified).
    notified_at TIMESTAMPTZ,
    UNIQUE (manhwa_id, number)
);
CREATE INDEX idx_chapter_manhwa ON chapter(manhwa_id);
CREATE INDEX idx_chapter_unnotified ON chapter(manhwa_id) WHERE notified_at IS NULL;

CREATE TABLE progress (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    chapter_id   BIGINT NOT NULL UNIQUE REFERENCES chapter(id) ON DELETE CASCADE,
    started_at   TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    scrolled_pct DOUBLE PRECISION NOT NULL DEFAULT 0.0
);
CREATE INDEX idx_progress_chapter ON progress(chapter_id);

CREATE TABLE discovered_manhwa (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source         TEXT NOT NULL,
    source_url     TEXT NOT NULL UNIQUE,
    title          TEXT NOT NULL,
    cover_url      TEXT,
    chapter_number DOUBLE PRECISION,
    released_at    TEXT,
    first_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    dismissed      BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE INDEX idx_discovered_undismissed ON discovered_manhwa(dismissed, first_seen_at DESC);

CREATE TABLE discovery_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE manhwa_alias (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    manhwa_id  BIGINT NOT NULL REFERENCES manhwa(id) ON DELETE CASCADE,
    source     TEXT NOT NULL,
    source_url TEXT NOT NULL UNIQUE,
    added_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (manhwa_id, source)
);
CREATE INDEX idx_alias_by_manhwa ON manhwa_alias(manhwa_id);
