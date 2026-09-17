"""Offline derived evidence for ci_timings; never reads logs or contacts GitHub."""

import collections
import hashlib
import json
import re


def execution_evidence(tables, stamp):
    """New API IDs can carry prior execution timestamps through failed-job reruns."""
    attempts = {(a["run_id"], a["attempt"]): a for a in tables["attempts"]}
    seen = {}
    records = []
    keep = set()
    for job in sorted(tables["jobs"], key=lambda j: (j["run_id"], int(j["attempt"]), j["job_id"])):
        start, end = stamp(job["started_at"]), stamp(job["completed_at"])
        key = tuple(job[k] for k in ("run_id", "source_job", "name", "labels", "started_at", "completed_at", "conclusion"))
        valid = start is not None and end is not None and end >= start and job["conclusion"] != "skipped"
        previous = seen.get(key) if valid else None
        attempt_start = stamp(attempts[(job["run_id"], job["attempt"])]["started_at"])
        if previous and int(previous["attempt"]) >= int(job["attempt"]):
            previous = None
        canonical = previous or job
        if previous:
            classification = "carryover"
        elif valid and attempt_start and start < attempt_start:
            classification = "uncertain_pre_attempt"
        else:
            classification = "canonical_observed" if valid else "unmeasured"
            keep.add(job["job_id"])
        if valid and key not in seen:
            seen[key] = job
        records.append(dict(job_id=job["job_id"], canonical_job_id=canonical["job_id"],
                            earliest_observed_attempt=canonical["attempt"], classification=classification))
    filtered = dict(tables)
    filtered["jobs"] = [j for j in tables["jobs"] if j["job_id"] in keep]
    for name in ("steps", "metrics"):
        filtered[name] = [row for row in tables[name] if row["job_id"] in keep]
    return filtered, records


def workflow_references(texts, source_jobs):
    references = {}
    for source_id, text in sorted(texts.items()):
        lines = text.splitlines()
        definitions = source_jobs(text)
        jobs = {}
        for index, job in enumerate(definitions):
            end = definitions[index + 1]["line"] - 1 if index + 1 < len(definitions) else len(lines)
            block = lines[job["line"] - 1:end]
            needs = next((line.removeprefix("    needs: ") for line in block if line.startswith("    needs: ")), "")
            if re.fullmatch(r"[\w-]+", needs):
                dependencies = [needs]
            elif re.fullmatch(r"\[[\w, -]+\]", needs):
                dependencies = [part.strip() for part in needs[1:-1].split(",")]
            else:
                dependencies = [] if not needs else None
            steps = []
            for number, step in enumerate(job["steps"]):
                stop = job["steps"][number + 1]["line"] - 1 if number + 1 < len(job["steps"]) else end
                steps.append(dict(name=step["name"], line=step["line"], end_line=stop))
            jobs[job["key"]] = dict(name=job["name"], line=job["line"], needs=dependencies, steps=steps)
        references[source_id] = jobs
    return references


def derive(tables, references, summary, duration):
    runs = {r["run_id"]: r for r in tables["runs"]}
    attempts = {(a["run_id"], a["attempt"]): a for a in tables["attempts"]}
    jobs = {j["job_id"]: j for j in tables["jobs"]}
    by_attempt = collections.defaultdict(list)
    by_job = collections.defaultdict(list)
    evidence_by_attempt = collections.defaultdict(list)
    for job in tables.get("observed_jobs", tables["jobs"]):
        evidence_by_attempt[(job["run_id"], job["attempt"])].append(job)
    for job in jobs.values():
        by_attempt[(job["run_id"], job["attempt"])].append(job)
    for step in tables["steps"]:
        by_job[step["job_id"]].append(step)

    # Executed named steps distinguish conditional package/install lanes without
    # pretending that the collector retained the classifier's boolean outputs.
    lanes = {}
    job_lanes = {}
    for job_id, job in jobs.items():
        names = [s["name"] for s in by_job[job_id] if s["source_line"] and s["duration_seconds"]]
        lane_id = hashlib.sha256(json.dumps(names, separators=(",", ":")).encode()).hexdigest()[:16]
        lanes[lane_id] = names
        job_lanes[job_id] = lane_id

    cohort_values = collections.defaultdict(list)
    metric_values = collections.defaultdict(list)
    metric_rows = collections.defaultdict(list)
    for metric in tables["metrics"]:
        metric_rows[(metric["job_id"], metric["metric"])].append(metric)
    cache = collections.defaultdict(list)
    for (job_id, metric), rows in metric_rows.items():
        job = jobs[job_id]
        if metric.startswith("sccache_"):
            cache[(job["name"], metric)].append(max(float(r["maximum"]) for r in rows))
        if metric in {"cargo_finished_seconds", "test_execution_seconds"}:
            for row in rows:
                step = next((s for s in by_job[job_id] if s["number"] == row["step_number"]), None)
                if step:
                    metric_values[(job["name"], step["name"], metric)].append(float(row["total"]))

    observations = []
    for key, attempt in attempts.items():
        selected = [j for j in by_attempt[key] if j["duration_seconds"]]
        successful_latest = attempt["conclusion"] == "success" and attempt["attempt"] == runs[key[0]]["latest_attempt"]
        for job in selected:
            if successful_latest and job["conclusion"] == "success":
                cohort = (attempt["source_id"], job["source_job"] or job["name"], job["labels"], job_lanes[job["job_id"]])
                cohort_values[cohort].append((job["started_at"], job["job_id"], float(job["duration_seconds"])))
        definitions = references.get(attempt["source_id"], {})
        job_keys = {j["source_job"]: j for j in evidence_by_attempt[key] if j["source_job"]}
        gaps = []
        for job in selected:
            needs = definitions.get(job["source_job"], {}).get("needs")
            if not needs:
                continue
            dependencies = [job_keys.get(name) for name in needs]
            # Skipped jobs have sentinel times; omit their edge rather than treating
            # a fabricated zero timestamp as a real dependency completion.
            active = [j for j in dependencies if j and j["conclusion"] != "skipped"]
            if any(j is None for j in dependencies) or not active or any(not j["duration_seconds"] for j in active):
                continue
            ready = max(active, key=lambda j: j["completed_at"])
            gaps.append(dict(job_id=job["job_id"], job=job["source_job"], last_dependency=ready["source_job"],
                             seconds=duration(max(ready["completed_at"], attempt["started_at"]), job["started_at"])))
        # Parallel branch finishes exclude classifier/aggregator, whose execution
        # is not competing parallel work. This tail is not a counterfactual saving.
        branches = sorted((j for j in selected if j["source_job"] not in {"changes", "quality"}),
                          key=lambda j: (j["completed_at"], j["job_id"]))
        observations.append(dict(run_id=key[0], attempt=key[1], conclusion=attempt["conclusion"],
            latest=attempt["attempt"] == runs[key[0]]["latest_attempt"],
            elapsed_seconds=float(attempt["elapsed_seconds"]) if attempt["elapsed_seconds"] else None,
            job_seconds=sum(float(j["duration_seconds"]) for j in selected),
            branch_terminal=branches[-1]["source_job"] if branches else None,
            branch_tail_seconds=duration(branches[-2]["completed_at"], branches[-1]["completed_at"]) if len(branches) > 1 else None,
            dependency_gaps=gaps))

    cohorts = []
    for key, values in sorted(cohort_values.items()):
        values.sort()
        midpoint = len(values) // 2
        cohorts.append(dict(source_id=key[0], job=key[1], platform=key[2], lane_id=key[3], n=len(values),
            early=summary([v for _, _, v in values[:midpoint]]), recent=summary([v for _, _, v in values[midpoint:]]),
            early_job_ids=[j for _, j, _ in values[:midpoint]], recent_job_ids=[j for _, j, _ in values[midpoint:]]))

    def measured(rows, field):
        return summary([r[field] for r in rows if r[field] is not None])

    latest = [o for o in observations if o["latest"]]
    success = [o for o in latest if o["conclusion"] == "success"]
    rust = [o for o in success if any(j["source_job"] == "test" and j["duration_seconds"] for j in by_attempt[(o["run_id"], o["attempt"])] )]
    elapsed = {}
    for name, rows in (("all_attempts", observations), ("latest_attempts", latest), ("successful_latest", success), ("successful_latest_rust", rust), ("successful_latest_non_rust", [o for o in success if o not in rust])):
        elapsed[name] = dict(elapsed_seconds=measured(rows, "elapsed_seconds"), job_seconds=measured(rows, "job_seconds"),
                             branch_terminals=dict(collections.Counter(o["branch_terminal"] or "unobserved" for o in rows)))
    gaps = collections.defaultdict(list)
    for o in success:
        for gap in o["dependency_gaps"]:
            if gap["seconds"] is not None:
                gaps[gap["job"]].append(gap["seconds"])
    registry_jobs = {job_id for (job_id, metric) in metric_rows if metric == "cache_restore_messages"}
    restore_steps = [s for s in tables["steps"] if s["name"] == "Restore Cargo registry" and s["duration_seconds"]]
    frozen_ids = {str(r["run_id"]) for r in tables.get("frozen_runs", [])}
    retry_rows = []
    for run_id, run in runs.items():
        if int(run["latest_attempt"]) > 1:
            selected = [o for o in observations if o["run_id"] == run_id]
            final = attempts[(run_id, run["latest_attempt"])]
            retry_rows.append(dict(run_id=run_id, conclusion=run["conclusion"], attempts=len(selected),
                job_seconds=sum(o["job_seconds"] for o in selected),
                prior_job_seconds=sum(o["job_seconds"] for o in selected if not o["latest"]),
                created_to_final_completion_seconds=duration(run["created_at"], final["completed_at"])))
    return dict(
        reconciliation=dict(frozen=len(frozen_ids), extracted=len(runs), missing=sorted(frozen_ids - runs.keys()),
                            unexpected=sorted(runs.keys() - frozen_ids), incomplete=sorted(r["run_id"] for r in runs.values() if r["extraction"] != "complete")),
        elapsed=elapsed, observations=observations, successful_latest_dependency_gaps={k: summary(v) for k, v in sorted(gaps.items())},
        successful_latest_rust_branch_tail=measured(rust, "branch_tail_seconds"),
        lane_definitions=dict(sorted(lanes.items())), controlled_cohorts=cohorts, retries=retry_rows,
        assigned_metric_totals=[dict(job=k[0], step=k[1], metric=k[2], **summary(v)) for k, v in sorted(metric_values.items())],
        cache=dict(registry_restore_steps=len(restore_steps), jobs_with_restore_message=len(registry_jobs),
                   jobs_with_miss_message=sum(metric == "cache_miss_messages" for _, metric in metric_rows),
                   sccache_job_maxima=[dict(job=k[0], metric=k[1], zero_jobs=sum(value == 0 for value in v), **summary(v)) for k, v in sorted(cache.items())]),
    )
