ALTER TABLE outbound_contact_requests RENAME TO outbound_contact_request;
DROP INDEX outbound_contact_requests_pending_target;
CREATE UNIQUE INDEX outbound_contact_request_pending_target
    ON outbound_contact_request(target_name) WHERE status IN ('queued', 'pending');
