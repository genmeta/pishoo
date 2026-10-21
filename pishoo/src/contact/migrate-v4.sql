-- NULL means the original operation snapshot is unknown. Never infer it from
-- a new permission catalog: that could turn a historical POST grant into PUT.
ALTER TABLE outbound_contact_request ADD COLUMN requested_access TEXT
    CHECK (requested_access IS NULL OR (json_valid(requested_access) AND json_type(requested_access) = 'object'));
ALTER TABLE outbound_contact_request ADD COLUMN offers TEXT
    CHECK (offers IS NULL OR (json_valid(offers) AND json_type(offers) = 'object'));
