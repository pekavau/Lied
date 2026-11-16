-- Whether this org's musicians may read the whole archive (#119): every
-- arrangement, voice and full score, and their files, read-only. Off by
-- default: a musician reads only the parts assigned to them, as the
-- permission matrix says. Whether to open the archive depends on the
-- ensemble (its size, how professional it is), so the owner decides.
-- Guests never get it: they read only their assignments either way.

ALTER TABLE organization
    ADD COLUMN musicians_read_archive boolean NOT NULL DEFAULT false;
