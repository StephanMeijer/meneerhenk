-- What each session read from and wrote into the model's prompt cache
-- (#330, #136). input_tokens stays the whole prompt; these say how much of
-- it came from the cache and went into it. Sessions from before read 0.
ALTER TABLE lanes ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE lanes ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
