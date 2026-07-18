-- The project-local skill manifest is retired. Historical diagnostics which
-- instructed operators to reconcile it must not survive as a supported
-- diagnostic surface after the cut-over.
DELETE FROM extension_load_diagnostics
WHERE phase = 'manifest_drift'
   OR remedy_code = 'update_skill_manifest';

ALTER TABLE extension_load_diagnostics
    DROP CONSTRAINT chk_extension_load_diagnostics_phase,
    DROP CONSTRAINT chk_extension_load_diagnostics_remedy_code;

ALTER TABLE extension_load_diagnostics
    ADD CONSTRAINT chk_extension_load_diagnostics_phase
        CHECK (phase IN ('placeholder_resolution', 'process_start', 'transport', 'handshake', 'tools_list', 'frontmatter', 'missing_file')),
    ADD CONSTRAINT chk_extension_load_diagnostics_remedy_code
        CHECK (remedy_code IN ('check_placeholder', 'check_command', 'check_transport', 'check_server', 'check_skill_frontmatter', 'restore_skill_file'));
