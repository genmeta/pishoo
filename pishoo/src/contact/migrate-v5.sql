ALTER TABLE outbound_contact_request ADD COLUMN review_id INTEGER CHECK (review_id > 0);
ALTER TABLE outbound_contact_request ADD COLUMN review_subject TEXT;
ALTER TABLE outbound_contact_request ADD COLUMN review_method TEXT;
ALTER TABLE outbound_contact_request ADD COLUMN review_path TEXT;
