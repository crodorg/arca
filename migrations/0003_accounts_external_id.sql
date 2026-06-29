-- Provider-side account ID lookup. Nullable; manual accounts leave it NULL.
ALTER TABLE accounts ADD COLUMN external_id TEXT;
CREATE INDEX idx_accounts_external ON accounts(provider_id, external_id);
