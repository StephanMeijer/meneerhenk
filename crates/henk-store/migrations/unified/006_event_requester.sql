-- Who asked for an event, when its source knows: the API's requester or
-- `github:<id>` from the dashboard (#69).
ALTER TABLE inbound_events ADD COLUMN requester TEXT;
