-- Pasted images (card #67).
--
-- One row per upload. A card body refers to an image only by its URL
-- (`/api/images/{id}`), so there is deliberately no `card_id` and no foreign
-- key: cut and paste can carry that markdown into any card on any board, and
-- an image must keep serving after every card that shows it has been
-- deleted, because history restore can bring any old body back.
--
-- Rows are immutable and never deleted: the bytes behind an id never change,
-- which is what lets `GET /api/images/{id}` tell browsers to cache them
-- forever. The upload route caps `bytes` at 10 MiB.
--
-- Additive only: release N-1 never reads this table, so it runs unchanged
-- against a database that has it (the `migrations` skill's compatibility
-- rule, card #640).
CREATE TABLE images (
    -- A UUIDv4 the server generates: 122 random bits, unguessable, and
    -- revealing nothing (not even the upload time, which a ULID would).
    id           uuid        PRIMARY KEY,
    -- The MIME type detected from the bytes themselves, never the client's
    -- header: one of image/png, image/jpeg, image/gif, image/webp,
    -- image/svg+xml.
    content_type text        NOT NULL,
    bytes        bytea       NOT NULL,
    -- `octet_length(bytes)`, stored so a future listing need not read them.
    size         integer     NOT NULL CHECK (size >= 0),
    -- The uploader's OIDC `sub`, like `cards.last_edited_by`.
    created_by   text        NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
