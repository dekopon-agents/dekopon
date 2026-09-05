-- Additive repair: retained schema/evidence v1..v3 are never rewritten.
PRAGMA user_version = 4;
ALTER TABLE prerequisite_evidence RENAME TO legacy_prerequisite_evidence;
CREATE TABLE prerequisite_evidence(prerequisite_id TEXT PRIMARY KEY NOT NULL REFERENCES prerequisite, satisfied INTEGER NOT NULL CHECK(satisfied IN (0,1)), evidence TEXT NOT NULL REFERENCES artifact, updated_at TEXT NOT NULL);
INSERT INTO prerequisite_evidence SELECT * FROM legacy_prerequisite_evidence;
DROP TABLE legacy_prerequisite_evidence;
DROP VIEW unresolved;

CREATE TABLE binary_version(role TEXT PRIMARY KEY NOT NULL CHECK(role IN ('collector','history')), input_sha256 TEXT NOT NULL CHECK(length(input_sha256)=64));
CREATE TABLE binary_requirement(consumer_build_id TEXT NOT NULL REFERENCES build, role TEXT NOT NULL REFERENCES binary_version, producer_build_id TEXT NOT NULL REFERENCES build, source_id TEXT REFERENCES source_revision, PRIMARY KEY(consumer_build_id,role));
CREATE UNIQUE INDEX artifact_owner ON artifact(id,attempt_id);
CREATE TABLE binary_product(artifact_id TEXT PRIMARY KEY NOT NULL, producer_attempt_id TEXT NOT NULL REFERENCES attempt, role TEXT NOT NULL REFERENCES binary_version, build_id TEXT NOT NULL REFERENCES build, source_id TEXT REFERENCES source_revision, input_sha256 TEXT NOT NULL CHECK(length(input_sha256)=64), source_artifact_id TEXT NOT NULL, build_log_artifact_id TEXT NOT NULL,
 FOREIGN KEY(artifact_id,producer_attempt_id) REFERENCES artifact(id,attempt_id),
 FOREIGN KEY(source_artifact_id,producer_attempt_id) REFERENCES artifact(id,attempt_id),
 FOREIGN KEY(build_log_artifact_id,producer_attempt_id) REFERENCES artifact(id,attempt_id));
CREATE TRIGGER product_producer BEFORE INSERT ON binary_product WHEN NOT EXISTS(SELECT 1 FROM attempt a JOIN trial t ON t.id=a.trial_id JOIN experiment e ON e.id=t.experiment_id JOIN artifact f ON f.id=NEW.artifact_id WHERE a.id=NEW.producer_attempt_id AND e.build_id=NEW.build_id AND e.recipe_id='build-'||NEW.role AND f.kind='binary') BEGIN SELECT RAISE(ABORT,'incompatible binary producer'); END;
CREATE TRIGGER immutable_product BEFORE UPDATE ON binary_product BEGIN SELECT RAISE(ABORT,'immutable binary provenance'); END;
CREATE TRIGGER retained_product BEFORE DELETE ON binary_product BEGIN SELECT RAISE(ABORT,'retain binary provenance'); END;
CREATE TABLE attempt_input(attempt_id TEXT NOT NULL REFERENCES attempt, role TEXT NOT NULL REFERENCES binary_version, artifact_id TEXT NOT NULL REFERENCES binary_product, PRIMARY KEY(attempt_id,role));
CREATE TRIGGER compatible_input BEFORE INSERT ON attempt_input WHEN NOT EXISTS(SELECT 1 FROM attempt a JOIN trial t ON t.id=a.trial_id JOIN experiment e ON e.id=t.experiment_id JOIN binary_requirement r ON r.consumer_build_id=e.build_id AND r.role=NEW.role JOIN binary_version v ON v.role=r.role JOIN binary_product p ON p.artifact_id=NEW.artifact_id JOIN attempt producer ON producer.id=p.producer_attempt_id WHERE a.id=NEW.attempt_id AND p.role=r.role AND p.build_id=r.producer_build_id AND p.source_id IS r.source_id AND p.input_sha256=v.input_sha256 AND producer.status='succeeded') BEGIN SELECT RAISE(ABORT,'incompatible binary input'); END;
CREATE TRIGGER immutable_input BEFORE UPDATE ON attempt_input BEGIN SELECT RAISE(ABORT,'immutable input link'); END;
CREATE TRIGGER retained_input BEFORE DELETE ON attempt_input BEGIN SELECT RAISE(ABORT,'retain input link'); END;
CREATE VIEW unresolved AS SELECT e.id,e.lane,e.reason,p.id AS prerequisite,p.description,CASE WHEN p.kind='artifact' THEN EXISTS(SELECT 1 FROM binary_requirement r JOIN binary_version v ON v.role=r.role JOIN binary_product b ON b.role=r.role AND b.build_id=r.producer_build_id AND b.source_id IS r.source_id AND b.input_sha256=v.input_sha256 JOIN attempt a ON a.id=b.producer_attempt_id WHERE r.consumer_build_id=e.build_id AND r.role=p.id AND a.status='succeeded') ELSE COALESCE(pe.satisfied,0) END AS satisfied FROM experiment e LEFT JOIN experiment_prerequisite ep ON ep.experiment_id=e.id LEFT JOIN prerequisite p ON p.id=ep.prerequisite_id LEFT JOIN prerequisite_evidence pe ON pe.prerequisite_id=p.id;

ALTER TABLE error ADD COLUMN role TEXT NOT NULL DEFAULT 'primary' CHECK(role IN ('primary','cleanup'));
UPDATE error SET role='cleanup' WHERE category='ownership';
CREATE TABLE attempt_outcome(attempt_id TEXT PRIMARY KEY NOT NULL REFERENCES attempt, status TEXT NOT NULL CHECK(status IN ('failed','blocked','interrupted','succeeded')), exit_code INTEGER, error_id INTEGER REFERENCES error, recorded_at TEXT NOT NULL);
CREATE TRIGGER immutable_outcome BEFORE UPDATE ON attempt_outcome BEGIN SELECT RAISE(ABORT,'retain primary outcome'); END;
CREATE TABLE trial_incident(id INTEGER PRIMARY KEY, trial_id TEXT NOT NULL REFERENCES trial, stage TEXT NOT NULL, category TEXT NOT NULL, message TEXT NOT NULL, at TEXT NOT NULL);
CREATE TABLE execution_sequence(sequence INTEGER PRIMARY KEY AUTOINCREMENT, attempt_id TEXT NOT NULL UNIQUE REFERENCES attempt, claimed_at TEXT NOT NULL, launch_sequence INTEGER UNIQUE, dispatched_at TEXT, start_observed_at TEXT);
ALTER TABLE sample ADD COLUMN clock_origin TEXT NOT NULL DEFAULT 'legacy-unspecified';
CREATE TABLE parse_scope(parse_id TEXT PRIMARY KEY NOT NULL REFERENCES parse_run, memory_scope TEXT NOT NULL CHECK(memory_scope IN ('whole-run-only','history-synchronized','recipe-phases')), protocol TEXT NOT NULL);

DROP VIEW results;
CREATE VIEW results AS SELECT t.campaign_id,t.experiment_id,t.replicate,a.id AS attempt_id,a.status,e.architecture,e.page_bytes,COALESCE(scope.memory_scope,'whole-run-only') AS memory_scope,s.*,m.unit FROM phase_summary s JOIN latest_parse p ON p.id=s.parse_id JOIN attempt a ON a.id=p.attempt_id JOIN trial t ON t.id=a.trial_id LEFT JOIN environment e ON e.id=a.environment_id LEFT JOIN parse_scope scope ON scope.parse_id=p.id JOIN metric m ON m.id=s.metric_id;
