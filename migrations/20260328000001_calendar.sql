CREATE TABLE IF NOT EXISTS calendar_sources (
    source_id TEXT PRIMARY KEY,
    provider_kind TEXT NOT NULL,
    base_url TEXT,
    principal_url TEXT,
    home_set_url TEXT,
    auth_kind TEXT NOT NULL,
    last_discovery_at TEXT,
    last_sync_at TEXT,
    last_successful_sync_at TEXT,
    last_error TEXT,
    sync_status TEXT
);

CREATE TABLE IF NOT EXISTS calendar_calendars (
    href TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    display_name TEXT,
    description TEXT,
    color TEXT,
    timezone TEXT,
    ctag TEXT,
    sync_token TEXT,
    is_selected INTEGER NOT NULL DEFAULT 0,
    discovered_at TEXT NOT NULL,
    last_synced_at TEXT,
    FOREIGN KEY (source_id) REFERENCES calendar_sources(source_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_calendar_calendars_source
    ON calendar_calendars(source_id, is_selected);

CREATE TABLE IF NOT EXISTS calendar_resources (
    id TEXT PRIMARY KEY,
    calendar_href TEXT NOT NULL,
    remote_href TEXT NOT NULL UNIQUE,
    etag TEXT,
    raw_ics TEXT NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (calendar_href) REFERENCES calendar_calendars(href) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_calendar_resources_calendar
    ON calendar_resources(calendar_href, deleted);

CREATE TABLE IF NOT EXISTS calendar_events (
    id TEXT PRIMARY KEY,
    resource_id TEXT NOT NULL,
    calendar_href TEXT NOT NULL,
    remote_uid TEXT NOT NULL,
    recurrence_id_utc TEXT,
    summary TEXT,
    description TEXT,
    location TEXT,
    status TEXT,
    organizer_name TEXT,
    organizer_email TEXT,
    start_at_utc TEXT NOT NULL,
    end_at_utc TEXT NOT NULL,
    timezone TEXT,
    all_day INTEGER NOT NULL DEFAULT 0,
    recurrence_rule TEXT,
    recurrence_exdates_json TEXT,
    sequence INTEGER NOT NULL DEFAULT 0,
    transparency TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (resource_id) REFERENCES calendar_resources(id) ON DELETE CASCADE,
    FOREIGN KEY (calendar_href) REFERENCES calendar_calendars(href) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_events_resource_instance
    ON calendar_events(resource_id, remote_uid, COALESCE(recurrence_id_utc, ''));

CREATE INDEX IF NOT EXISTS idx_calendar_events_calendar_time
    ON calendar_events(calendar_href, start_at_utc);

CREATE INDEX IF NOT EXISTS idx_calendar_events_uid
    ON calendar_events(calendar_href, remote_uid);

CREATE TABLE IF NOT EXISTS calendar_attendees (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL,
    email TEXT,
    common_name TEXT,
    role TEXT,
    partstat TEXT,
    rsvp INTEGER NOT NULL DEFAULT 0,
    is_organizer INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (event_id) REFERENCES calendar_events(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_calendar_attendees_event
    ON calendar_attendees(event_id);

CREATE TABLE IF NOT EXISTS calendar_change_proposals (
    id TEXT PRIMARY KEY,
    action TEXT NOT NULL,
    status TEXT NOT NULL,
    event_id TEXT,
    summary TEXT NOT NULL,
    diff TEXT NOT NULL,
    basis_etag TEXT,
    draft_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    applied_at TEXT,
    error TEXT,
    FOREIGN KEY (event_id) REFERENCES calendar_events(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_calendar_change_proposals_status
    ON calendar_change_proposals(status, created_at);
