-- The compiled host release authorizes this exact historical transfer.
-- Keep all historical signature, digest, timestamp and content facts intact.
UPDATE package SET author=?2, author_owner_hash=?3 WHERE name=?1;
