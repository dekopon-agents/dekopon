-- Optional foreign keys need separate partial uniqueness (NULLs are distinct in SQLite).
PRAGMA user_version = 3;
CREATE UNIQUE INDEX finding_reference_once ON finding_evidence(finding_id,reference_id) WHERE reference_id IS NOT NULL;
CREATE UNIQUE INDEX finding_artifact_once ON finding_evidence(finding_id,artifact_id) WHERE artifact_id IS NOT NULL;
CREATE UNIQUE INDEX finding_parse_once ON finding_evidence(finding_id,parse_id) WHERE parse_id IS NOT NULL;
