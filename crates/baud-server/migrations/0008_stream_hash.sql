-- baud-server: add observation stream hash to runs table for replay verification
-- Adds stream_hash column so verify/replay can compare hashes rather than counts.

ALTER TABLE runs ADD COLUMN stream_hash TEXT;
