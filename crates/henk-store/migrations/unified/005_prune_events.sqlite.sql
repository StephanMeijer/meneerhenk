-- Pruning deletes outcomes by their event (#69); PostgreSQL has had this
-- index since its first migration.
CREATE INDEX event_outcomes_event ON event_outcomes(event_id);
