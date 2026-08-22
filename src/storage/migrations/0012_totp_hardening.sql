-- TOTP replay protection: track the last accepted timestep per user so a
-- code can never be accepted twice.
ALTER TABLE users ADD COLUMN totp_last_step INTEGER;
