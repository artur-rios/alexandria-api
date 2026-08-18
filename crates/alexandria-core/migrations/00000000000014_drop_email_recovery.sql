-- Recovery codes replaced e-mail confirmation and password reset, so the
-- state they needed goes with them.
--
-- Nothing of value is dropped. `MailProvider` has only ever had one variant,
-- `None`, so every send was refused: no row in `auth_tokens` was ever
-- delivered to anyone, and `email_confirmed_at` is NULL on every install in
-- existence because nothing could ever have confirmed an address.
DROP TABLE IF EXISTS auth_tokens;

ALTER TABLE local_login_credentials DROP COLUMN email_confirmed_at;
