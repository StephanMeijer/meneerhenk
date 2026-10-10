-- Why a review loop stopped and after how many rounds (#286); empty for
-- every other run.
ALTER TABLE runs ADD COLUMN loop_stop TEXT;
ALTER TABLE runs ADD COLUMN loop_rounds INTEGER;
