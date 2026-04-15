-- Mirror of the MySQL V2 migration. SQLite already stores these as TEXT
-- via affinity, but we emit this migration so the two backends stay in
-- lock-step (and `sqlx::migrate!` checksums catch drift).
--
-- No-op on SQLite: TEXT is already the storage class for the existing
-- columns, and SQLite permits reading it as String. Present so the
-- version numbers match MySQL.
SELECT 1;
