-- Blob files under {app_data_dir}/file-journal/ are deliberately left alone:
-- a down migration reverses the schema, not the artefacts on disk.
DROP TABLE journal_versions;
DROP TABLE journal_blobs;
DROP TABLE journal_files;
