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
DEFINE FIELD IF NOT EXISTS column      ON TABLE cards TYPE record<columns>;
DEFINE FIELD IF NOT EXISTS title       ON TABLE cards TYPE string;
DEFINE FIELD IF NOT EXISTS description ON TABLE cards TYPE option<string>;
DEFINE FIELD IF NOT EXISTS position    ON TABLE cards TYPE int;
DEFINE FIELD IF NOT EXISTS created_at  ON TABLE cards TYPE datetime DEFAULT time::now();
DEFINE FIELD IF NOT EXISTS updated_at  ON TABLE cards TYPE datetime VALUE time::now();
DEFINE INDEX IF NOT EXISTS cards_column ON TABLE cards FIELDS column;
