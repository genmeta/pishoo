-- Contact database migration v1 -> v2: bind requests to a contact generation.
ALTER TABLE outbound_contact_requests ADD COLUMN contact_id INTEGER;
ALTER TABLE outbound_contact_requests ADD COLUMN contact_subject TEXT;
DROP TABLE IF EXISTS saved_contacts;
