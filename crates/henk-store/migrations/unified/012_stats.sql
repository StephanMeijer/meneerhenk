-- The overview counts findings and drafts per day (#225).
CREATE INDEX findings_created ON findings(created_at);
CREATE INDEX drafts_created ON drafts(created_at);
