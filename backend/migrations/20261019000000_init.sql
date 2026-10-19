-- bored's schema on Postgres (card #471), translated from the SurrealDB
-- `schema.surql` it replaces. Applied by `sqlx::migrate!` at startup
-- (`db::connect`); sqlx records it in `_sqlx_migrations` and never runs it
-- twice.
--
-- Ids are the lower-case ULIDs the API has always used, stored as text.
-- Timestamps are `timestamptz`, whose microsecond precision is exactly the
-- API's wire format (card #472, `backend/src/timestamp.rs`).

-- ── updated_at ───────────────────────────────────────────────────────────────
-- SurrealDB's `updated_at ... VALUE time::now()` restamped the field on every
-- write. A BEFORE UPDATE trigger is the Postgres equivalent, so no query has to
-- remember to set it. `clock_timestamp()` rather than `now()`: `now()` is the
-- transaction's start time and would not move between two updates in one
-- transaction.
CREATE FUNCTION touch_updated_at() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := clock_timestamp();
    RETURN NEW;
END $$;

-- ── boards ───────────────────────────────────────────────────────────────────
CREATE TABLE boards (
    id             text PRIMARY KEY,
    -- The board's URL slug. The constraint name is what `error.rs` matches to
    -- turn a duplicate into a 409.
    name           text NOT NULL CONSTRAINT board_name_unique UNIQUE,
    last_edited_by text,
    -- `now()` (the transaction's time) for both, so a fresh row's two
    -- timestamps are identical, as they were under SurrealDB.
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER boards_touch_updated_at BEFORE UPDATE ON boards
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- ── columns ──────────────────────────────────────────────────────────────────
-- ON DELETE RESTRICT, deliberately: the board-delete route removes a board's
-- columns itself, writing an audit row for each. A code path that forgot to
-- would fail loudly here instead of silently deleting columns with no history.
CREATE TABLE columns (
    id             text PRIMARY KEY,
    board_id       text NOT NULL REFERENCES boards (id) ON DELETE RESTRICT,
    name           text NOT NULL,
    position       integer NOT NULL,
    last_edited_by text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX columns_board_position ON columns (board_id, position);
CREATE TRIGGER columns_touch_updated_at BEFORE UPDATE ON columns
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- ── cards ────────────────────────────────────────────────────────────────────
-- Card numbers come from a sequence: `nextval` is atomic, so concurrent creates
-- never share a number, and a rolled-back create leaves a gap rather than a
-- duplicate (numbers are gap-tolerant by design). The one-off SurrealDB import
-- `setval`s it to the old counter's high-water mark.
CREATE SEQUENCE card_number_seq AS integer;

-- ON DELETE RESTRICT for the same reason as `columns`: the column- and
-- board-delete routes remove (and audit) the cards themselves.
CREATE TABLE cards (
    id             text PRIMARY KEY,
    column_id      text NOT NULL REFERENCES columns (id) ON DELETE RESTRICT,
    body           text NOT NULL,
    position       integer NOT NULL,
    -- Unique: a number names one card. An audit restore re-inserts a deleted
    -- card under its old number, which is free again by then.
    number         integer NOT NULL DEFAULT nextval('card_number_seq')
                   CONSTRAINT cards_number_unique UNIQUE,
    tags           text[] NOT NULL DEFAULT '{}',
    last_edited_by text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
ALTER SEQUENCE card_number_seq OWNED BY cards.number;
CREATE INDEX cards_column_position ON cards (column_id, position);
CREATE TRIGGER cards_touch_updated_at BEFORE UPDATE ON cards
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- ── card_links ───────────────────────────────────────────────────────────────
-- One row is one fact: `predecessor` comes before `successor` (card #76).
-- ON DELETE CASCADE is a backstop only: every route that deletes a card first
-- deletes (and audits) its links (`links::cascade_delete_card_links`), so in
-- practice nothing is left for the cascade to remove. Unlike columns and
-- cards, a link has no history of its own worth failing a delete over.
CREATE TABLE card_links (
    id             text PRIMARY KEY,
    predecessor_id text NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
    successor_id   text NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
    reason         text,
    last_edited_by text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    -- An exact duplicate is a 409 (`error.rs` matches this name). The reverse
    -- pair is a cycle, which the application rejects before writing.
    CONSTRAINT card_links_pair UNIQUE (predecessor_id, successor_id)
);
-- The pair constraint's index already serves lookups by predecessor.
CREATE INDEX card_links_successor ON card_links (successor_id);
CREATE TRIGGER card_links_touch_updated_at BEFORE UPDATE ON card_links
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- ── audit_log ────────────────────────────────────────────────────────────────
-- Append-only history (iteration 21, card #127). No foreign keys: history
-- outlives the entities it describes.
CREATE TABLE audit_log (
    id                 text PRIMARY KEY,
    actor_sub          text NOT NULL,
    actor_display_name text NOT NULL,
    entity_type        text NOT NULL,
    entity_id          text NOT NULL,
    board_id           text NOT NULL,
    action             text NOT NULL,
    snapshot_before    jsonb,
    snapshot_after     jsonb,
    restored_from      text,
    batch_group        text,
    audit_edit_session text,
    -- `clock_timestamp()`, not `now()`: history is ordered by this column, and
    -- several rows written in one transaction must still be told apart.
    created_at         timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX audit_board_time ON audit_log (board_id, created_at);
CREATE INDEX audit_entity_time ON audit_log (entity_type, entity_id, created_at);
CREATE INDEX audit_batch_group ON audit_log (batch_group) WHERE batch_group IS NOT NULL;
