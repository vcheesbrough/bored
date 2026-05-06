DEFINE TABLE IF NOT EXISTS boards SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS name           ON TABLE boards TYPE string;
DEFINE FIELD IF NOT EXISTS last_edited_by ON TABLE boards TYPE option<string>;
DEFINE FIELD IF NOT EXISTS created_at     ON TABLE boards TYPE datetime DEFAULT time::now();
DEFINE FIELD IF NOT EXISTS updated_at     ON TABLE boards TYPE datetime VALUE time::now();

DEFINE TABLE IF NOT EXISTS columns SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS board          ON TABLE columns TYPE record<boards>;
DEFINE FIELD IF NOT EXISTS name           ON TABLE columns TYPE string;
DEFINE FIELD IF NOT EXISTS position       ON TABLE columns TYPE int;
DEFINE FIELD IF NOT EXISTS last_edited_by ON TABLE columns TYPE option<string>;
DEFINE FIELD IF NOT EXISTS created_at     ON TABLE columns TYPE datetime DEFAULT time::now();
DEFINE FIELD IF NOT EXISTS updated_at     ON TABLE columns TYPE datetime VALUE time::now();
DEFINE INDEX IF NOT EXISTS columns_board ON TABLE columns FIELDS board;

DEFINE TABLE IF NOT EXISTS cards SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS column         ON TABLE cards TYPE record<columns>;
DEFINE FIELD IF NOT EXISTS body           ON TABLE cards TYPE string;
DEFINE FIELD IF NOT EXISTS position       ON TABLE cards TYPE int;
DEFINE FIELD IF NOT EXISTS number         ON TABLE cards TYPE int DEFAULT 0;
DEFINE FIELD IF NOT EXISTS last_edited_by ON TABLE cards TYPE option<string>;
DEFINE FIELD IF NOT EXISTS created_at     ON TABLE cards TYPE datetime DEFAULT time::now();
DEFINE FIELD IF NOT EXISTS updated_at     ON TABLE cards TYPE datetime VALUE time::now();
DEFINE INDEX IF NOT EXISTS cards_column ON TABLE cards FIELDS column;
-- Migrate iteration-5 records that have title/description but no body yet.
-- Concatenates the old fields so deserialisation of `body: String` doesn't fail.
UPDATE cards SET body = string::concat(title, '\n\n', description ?? '') WHERE body IS NONE;
-- Remove legacy fields from the iteration-5 schema; no-op if they don't exist.
REMOVE FIELD IF EXISTS title       ON TABLE cards;
REMOVE FIELD IF EXISTS description ON TABLE cards;

-- Global counter used to assign unique sequential card numbers.
-- The single record card_counter:global holds the current high-water mark.
DEFINE TABLE IF NOT EXISTS card_counter SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS count ON TABLE card_counter TYPE int DEFAULT 0;
-- Seed the counter only on first run; leave it untouched on restarts.
-- SELECT returns an empty array when the record doesn't exist yet.
LET $ctr_exists = (SELECT id FROM card_counter:global);
IF array::len($ctr_exists) = 0 {
    CREATE card_counter:global CONTENT { count: 0 };
};

-- Migration: assign sequential numbers (ordered by created_at) to any cards
-- that have no number yet (NONE from before this field existed) or number = 0.
-- Idempotent — cards with number > 0 are skipped.
FOR $card IN (SELECT id, created_at FROM cards WHERE number IS NONE OR number = 0 ORDER BY created_at ASC) {
    LET $ctr = (UPDATE card_counter:global SET count += 1 RETURN AFTER)[0];
    UPDATE $card.id SET number = $ctr.count;
};

-- Append-only audit trail for boards / columns / cards (iteration 21 — card #127).
DEFINE TABLE IF NOT EXISTS audit_log SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS actor_sub ON audit_log TYPE string;
DEFINE FIELD IF NOT EXISTS actor_display_name ON audit_log TYPE string;
DEFINE FIELD IF NOT EXISTS entity_type ON audit_log TYPE string;
DEFINE FIELD IF NOT EXISTS entity_id ON audit_log TYPE string;
DEFINE FIELD IF NOT EXISTS board_id ON audit_log TYPE string;
DEFINE FIELD IF NOT EXISTS action ON audit_log TYPE string;
DEFINE FIELD IF NOT EXISTS snapshot_before ON audit_log FLEXIBLE TYPE option<object>;
DEFINE FIELD IF NOT EXISTS snapshot_after ON audit_log FLEXIBLE TYPE option<object>;
DEFINE FIELD IF NOT EXISTS restored_from ON audit_log TYPE option<string>;
DEFINE FIELD IF NOT EXISTS batch_group ON audit_log TYPE option<string>;
DEFINE FIELD IF NOT EXISTS audit_edit_session ON audit_log TYPE option<string>;
-- DEFAULT allows backfill migrations to SET created_at from entity `updated_at`.
DEFINE FIELD OVERWRITE created_at ON audit_log TYPE datetime DEFAULT time::now();
DEFINE INDEX IF NOT EXISTS audit_board_time ON audit_log FIELDS board_id, created_at;
DEFINE INDEX IF NOT EXISTS audit_entity_time ON audit_log FIELDS entity_type, entity_id, created_at;
