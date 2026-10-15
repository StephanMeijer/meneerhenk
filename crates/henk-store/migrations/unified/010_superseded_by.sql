-- The run that replaced a superseded review: a review of a newer commit
-- of the same pull request (#231).
ALTER TABLE runs ADD COLUMN superseded_by TEXT;
