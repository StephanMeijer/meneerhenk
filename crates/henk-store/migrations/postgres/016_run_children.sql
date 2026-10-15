-- The runs a ticket run started (#536): its plan, its execute run and the
-- review of the pull request, one per stage. The child has no foreign key:
-- a review has no run row until it has a slot, and the link is made when it
-- is submitted.
CREATE TABLE run_children (
    parent TEXT NOT NULL REFERENCES runs(id),
    stage  TEXT NOT NULL,
    child  TEXT NOT NULL,
    PRIMARY KEY (parent, stage)
);
CREATE INDEX run_children_child ON run_children (child);
