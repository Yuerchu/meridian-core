-- SQLite does not support DROP COLUMN; reverting project_id requires table rebuild.
DROP TABLE IF EXISTS projects;
