-- A library has at most one unfinished index run (FR-FC-43), held by the
-- store rather than only by the handlers that check for one. Two requests can
-- both pass a handler's check — a double click, or the HTTP server and the
-- FFI host at once — and two walks of one folder race each other's inserts.
--
-- An upgraded database may already hold more than one for a library: runs
-- over folders inside a library were given that library by migration 28. The
-- newest stays; the rest are cancelled, as the owner cancelling them would
-- leave them, so the index below can be created.
UPDATE catalog_runs
   SET status = 'cancelled',
       finished_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
       phase = NULL
 WHERE library_id IS NOT NULL
   AND status IN ('running', 'paused')
   AND id <> (SELECT newest.id FROM catalog_runs newest
               WHERE newest.library_id = catalog_runs.library_id
                 AND newest.status IN ('running', 'paused')
               ORDER BY newest.started_at DESC, newest.id DESC
               LIMIT 1);

CREATE UNIQUE INDEX IF NOT EXISTS idx_catalog_runs_one_unfinished_per_library
    ON catalog_runs (library_id)
 WHERE library_id IS NOT NULL AND status IN ('running', 'paused');
