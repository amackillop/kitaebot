-- Tree identity lets a resumed review gate reuse its prior result.

ALTER TABLE reviews ADD COLUMN tree_hash TEXT;

CREATE INDEX reviews_tree_gate_idx
    ON reviews (repo, gate, tree_hash)
    WHERE tree_hash IS NOT NULL;
