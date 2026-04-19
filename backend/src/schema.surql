DEFINE TABLE IF NOT EXISTS boards SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS name       ON TABLE boards TYPE string;
DEFINE FIELD IF NOT EXISTS created_at ON TABLE boards TYPE datetime DEFAULT time::now();
DEFINE FIELD IF NOT EXISTS updated_at ON TABLE boards TYPE datetime VALUE time::now();

DEFINE TABLE IF NOT EXISTS columns SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS board      ON TABLE columns TYPE record<boards>;
DEFINE FIELD IF NOT EXISTS name       ON TABLE columns TYPE string;
DEFINE FIELD IF NOT EXISTS position   ON TABLE columns TYPE int;
DEFINE FIELD IF NOT EXISTS created_at ON TABLE columns TYPE datetime DEFAULT time::now();
DEFINE FIELD IF NOT EXISTS updated_at ON TABLE columns TYPE datetime VALUE time::now();
DEFINE INDEX IF NOT EXISTS columns_board ON TABLE columns FIELDS board;

DEFINE TABLE IF NOT EXISTS cards SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS column     ON TABLE cards TYPE record<columns>;
DEFINE FIELD IF NOT EXISTS body       ON TABLE cards TYPE string;
DEFINE FIELD IF NOT EXISTS position   ON TABLE cards TYPE int;
DEFINE FIELD IF NOT EXISTS number     ON TABLE cards TYPE int DEFAULT 0;
DEFINE FIELD IF NOT EXISTS created_at ON TABLE cards TYPE datetime DEFAULT time::now();
DEFINE FIELD IF NOT EXISTS updated_at ON TABLE cards TYPE datetime VALUE time::now();
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
-- Seed the counter record on first run; preserve existing count on re-runs.
-- `count ?? 0` returns 0 when the field is NONE (new record), existing value otherwise.
UPSERT card_counter:global SET count = count ?? 0;

-- Migration: assign sequential numbers (ordered by created_at) to any cards
-- that still have number = 0. Idempotent — numbered cards are skipped.
FOR $card IN (SELECT id, created_at FROM cards WHERE number = 0 ORDER BY created_at ASC) {
    LET $ctr = (UPDATE card_counter:global SET count += 1 RETURN AFTER)[0];
    UPDATE $card.id SET number = $ctr.count;
};
