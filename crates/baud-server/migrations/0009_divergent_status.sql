-- 0009_divergent_status.sql
-- Add 'divergent' as a valid run status.
-- SQLite does not enforce CHECK constraints on existing rows, so we only need
-- to document the extended status values here. Guards are enforced in Rust.
-- Status values: pending | provisioning | running | done | failed | aborted | divergent

-- No DDL change needed for SQLite — the TEXT column already accepts any string.
-- This migration is a documentation anchor for the 'divergent' status introduced at VR2-M15.
SELECT 1; -- no-op to make the file valid SQL
