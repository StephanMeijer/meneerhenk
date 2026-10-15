-- What each session spent reasoning before it answered (#365): part of
-- output_tokens, as the providers count it. Sessions from before read 0.
ALTER TABLE sessions ADD COLUMN reasoning_tokens BIGINT NOT NULL DEFAULT 0;
