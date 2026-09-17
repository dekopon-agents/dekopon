#!/usr/bin/env python3
"""Compact Actions timing evidence; stdlib only, gh owns authentication.

API timestamps are authoritative. Logs are streamed through a numeric allowlist,
never written or surfaced in errors. Historical source is evidence, not execution
SHA proof. See README.md for schema, bounds, and interpretation.
"""

import argparse
import base64
import collections
import csv
import datetime as dt
import hashlib
import json
import math
import os
from pathlib import Path
import re
import selectors
import statistics
import subprocess
import sys
import time
from urllib.parse import quote

SCHEMA = 1
MAX_JSON = 8 * 1024 * 1024
MAX_LOG = 64 * 1024 * 1024
MAX_LINE = 64 * 1024
TIMEOUT = 90
MAX_PAGES = 100


class SafeError(Exception):
    """Only locally authored codes and numeric HTTP statuses may cross this boundary."""


def utcnow():
    return dt.datetime.now(dt.timezone.utc).isoformat()


def stamp(value):
    if not value or value.startswith("0001-"):
        return None
    return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))


def duration(start, end, conclusion=None):
    if conclusion == "skipped":
        return None
    a, b = stamp(start), stamp(end)
    if a is None or b is None or b < a:
        return None
    return round((b - a).total_seconds(), 3)


def run_identity(value, repo):
    if value.isdigit():
        return repo, int(value)
    match = re.fullmatch(r"https://github\.com/([\w.-]+/[\w.-]+)/actions/runs/(\d+)(?:/attempts/\d+)?/?", value)
    if not match:
        raise SafeError("invalid_run_url_or_id")
    return match[1], int(match[2])


class Gh:
    def __init__(self):
        self.bytes = 0
        self.requests = 0

    def request(self, endpoint, parser=None):
        """One gh process at a time; bounded retries, wall clock, bytes and lines."""
        for attempt in range(3):
            if parser is not None:
                parser.reset()
            try:
                return self._request(endpoint, parser)
            except SafeError as exc:
                if str(exc) not in {"timeout", "http_429", "http_500", "http_502", "http_503", "http_504"} or attempt == 2:
                    raise
                time.sleep(2 ** attempt)

    def _request(self, endpoint, parser):
        env = {k: v for k, v in os.environ.items() if k not in {"GH_DEBUG", "DEBUG"}}
        env["GH_PROMPT_DISABLED"] = "1"
        self.requests += 1
        try:
            command = ["gh", "api", "--hostname", "github.com", "--include"]
            if parser is not None:
                # gh 2.100 rejects ANSI-bearing logs by default, even into a pipe.
                # This pipe never reaches a terminal; only numeric allowlist output does.
                command.append("--allow-escape-sequences")
            proc = subprocess.Popen(
                command + [endpoint],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
            )
        except OSError:
            raise SafeError("gh_unavailable") from None
        selector = selectors.DefaultSelector()
        selector.register(proc.stdout, selectors.EVENT_READ)
        selector.register(proc.stderr, selectors.EVENT_READ)
        stderr_kind = "unknown"
        stderr_categories = set()
        stderr_bytes = 0
        deadline = time.monotonic() + TIMEOUT
        pending = bytearray()
        body = bytearray()
        total = 0
        status = None
        headers = True
        overlong = False
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise SafeError("timeout")
                events = selector.select(min(remaining, 1))
                if not events:
                    continue
                key, _ = events[0]
                chunk = os.read(key.fileobj.fileno(), 16384)
                if not chunk:
                    selector.unregister(key.fileobj)
                    if not selector.get_map():
                        break
                    continue
                if key.fileobj is proc.stderr:
                    stderr_bytes += len(chunk)
                    if stderr_bytes > MAX_LINE:
                        raise SafeError("stderr_byte_limit")
                    # Fixed categories only; never retain or report stderr text/URLs.
                    for marker, kind in ((b"rate limit", "rate_limit"), (b"HTTP 403", "forbidden"),
                                         (b"HTTP 404", "not_found"), (b"HTTP 410", "gone"),
                                         (b"HTTP 401", "unauthorized"), (b"failed to authenticate", "unauthorized"),
                                         (b"timeout", "timeout"), (b"no such host", "dns"),
                                         (b"connection", "connection"), (b"TLS", "tls")):
                        if marker in chunk:
                            stderr_kind = kind
                    diagnostic_markers = {
                        "json": b"json", "binary": b"binary", "redirect": b"redirect",
                        "unsupported": b"unsupported", "content_type": b"content-type",
                        "eof": b"eof", "stream": b"stream", "http2": b"http2",
                        "gzip": b"gzip", "encoding": b"encoding", "parse": b"parse",
                        "write": b"write", "read": b"read", "broken_pipe": b"broken pipe",
                        "closed": b"closed", "invalid": b"invalid", "terminal": b"terminal",
                        "not_supported": b"not supported", "output": b"output",
                    }
                    stderr_categories.update(name for name, marker in diagnostic_markers.items() if marker in chunk.lower())
                    continue
                self.bytes += len(chunk)
                total += len(chunk)
                if total > (MAX_LOG if parser else MAX_JSON):
                    raise SafeError("response_byte_limit")
                pending.extend(chunk)
                while b"\n" in pending:
                    line, _, rest = pending.partition(b"\n")
                    pending = bytearray(rest)
                    if overlong:
                        overlong = False
                        continue
                    if headers:
                        match = re.match(rb"HTTP/\S+ (\d{3})", line)
                        if match:
                            status = int(match[1])
                        elif not line.strip():
                            headers = False
                        continue
                    if status != 200:
                        continue  # Never parse or expose an HTTP error body.
                    if parser:
                        parser.feed(line.decode("utf-8", "replace"))
                    else:
                        body.extend(line + b"\n")
                if headers or parser:
                    if len(pending) > MAX_LINE:
                        pending.clear()
                        overlong = True
                        if parser:
                            parser.overlong += 1
            code = proc.wait(timeout=max(0.01, deadline - time.monotonic()))
            if status != 200:
                if status:
                    raise SafeError(f"http_{status}")
                raise SafeError(f"gh_transport_exit_{code}_status_none_{stderr_kind}_bytes_{total}")
            if code:
                categories = "_".join(sorted(stderr_categories)) or "none"
                raise SafeError(f"gh_transport_exit_{code}_status_200_{stderr_kind}_bytes_{total}_stderr_bytes_{stderr_bytes}_categories_{categories}")
            if parser:
                if pending and not overlong:
                    parser.feed(pending.decode("utf-8", "replace"))
                return None
            body.extend(pending)
            try:
                return json.loads(body)
            except (ValueError, UnicodeError):
                raise SafeError("invalid_api_json") from None
        finally:
            if proc.poll() is None:
                proc.kill()
            proc.wait()
            proc.stdout.close()
            proc.stderr.close()
            selector.close()

    def pages(self, endpoint, key):
        separator = "&" if "?" in endpoint else "?"
        for page in range(1, MAX_PAGES + 1):
            result = self.request(f"{endpoint}{separator}per_page=100&page={page}")
            items = result[key]
            yield from items
            if len(items) < 100:
                return
        raise SafeError("pagination_limit")


class LogMetrics:
    """No free-text output. Match only fixed grammar; associate by API time window."""
    def __init__(self, steps):
        self.steps = steps
        self.reset()

    def reset(self):
        self.metrics = collections.defaultdict(list)
        self.checkouts = set()
        self.await_sha = None
        self.overlong = 0

    def feed(self, line):
        line = re.sub(r"\x1b\[[0-9;]*m", "", line).strip()
        match = re.match(r"(\d{4}-\d\d-\d\dT[\d:.]+Z) (.*)", line)
        if not match:
            self.await_sha = None
            return
        timestamp, text = match.groups()
        try:
            instant = stamp(timestamp)
        except ValueError:
            return
        candidates = [s for s in self.steps if s.get("conclusion") != "skipped"
                      and stamp(s.get("started_at")) and stamp(s.get("completed_at"))
                      and stamp(s["started_at"]) <= instant < stamp(s["completed_at"])]
        step = candidates[0] if len(candidates) == 1 else None
        number = step["number"] if step else 0
        # API step boundaries have only whole seconds; checkout often prints its
        # final SHA in the recorded completion second. Keep this tolerance local
        # to checkout provenance, not general metric attribution.
        checkout_steps = [s for s in self.steps if re.search(r"check\s*out|checkout", s["name"], re.I)
                          and stamp(s.get("started_at")) and stamp(s.get("completed_at"))
                          and stamp(s["started_at"]) <= instant < stamp(s["completed_at"]) + dt.timedelta(seconds=1)]
        checkout_step = checkout_steps[0]["number"] if len(checkout_steps) == 1 else None
        if self.await_sha is not None:
            if checkout_step == self.await_sha and re.fullmatch(r"[0-9a-f]{40}", text):
                self.checkouts.add(text)
            self.await_sha = None
        if checkout_step is not None and re.fullmatch(r"\[command\].*\bgit log -1 --format=['\"]?%H['\"]?", text):
            self.await_sha = checkout_step
        cargo = re.fullmatch(r"\s*Finished `(?:dev|test|release|bench)` profile \[.*\] target\(s\) in (?:(\d+)m )?([\d.]+)s", text)
        tests = re.fullmatch(r"test result: (?:ok|FAILED)\. \d+ passed; \d+ failed; \d+ ignored; \d+ measured; \d+ filtered out; finished in ([\d.]+)s", text)
        cache = re.fullmatch(r"(Compile requests|Compile requests executed|Cache hits|Cache misses|Non-cacheable calls|Cache read errors|Cache write errors)\s+(\d+)", text)
        if cargo:
            self.metrics[(number, "cargo_finished_seconds")].append(int(cargo[1] or 0) * 60 + float(cargo[2]))
        elif tests:
            self.metrics[(number, "test_execution_seconds")].append(float(tests[1]))
        elif cache:
            name = "sccache_" + cache[1].lower().replace(" ", "_")
            self.metrics[(number, name)].append(int(cache[2]))
        elif text.startswith("Cache restored from key:"):
            self.metrics[(number, "cache_restore_messages")].append(1)
        elif text.startswith("Cache not found for input keys:"):
            self.metrics[(number, "cache_miss_messages")].append(1)

    def rows(self, job_id):
        return [dict(job_id=job_id, step_number=step or None, metric=metric,
                     count=len(values), total=round(sum(values), 4),
                     minimum=min(values), maximum=max(values))
                for (step, metric), values in sorted(self.metrics.items())]


def source_jobs(text):
    """Conservative scanner for this repo's block-style workflows, not a YAML parser.

    Unknown/dynamic syntax remains unmapped. Full historical text is retained once
    per content hash so a reviewer can check every line reference independently.
    """
    lines = text.splitlines()
    starts = [(i, m[1]) for i, line in enumerate(lines)
              if (m := re.fullmatch(r"  ([\w-]+):", line))]
    result = []
    for index, (start, key) in enumerate(starts):
        end = starts[index + 1][0] if index + 1 < len(starts) else len(lines)
        block = lines[start:end]
        if "    steps:" not in block:
            continue
        name = next((line[10:].strip("'\"") for line in block if line.startswith("    name: ")), key)
        steps = []
        for i in range(start, end):
            match = re.match(r"      - name: (.+)", lines[i])
            if match:
                steps.append(dict(name=match[1].strip("'\""), line=i + 1))
        result.append(dict(key=key, name=name, line=start + 1, steps=steps))
    return result


def map_job(name, definitions):
    exact = [j for j in definitions if j["name"] == name]
    if len(exact) == 1:
        return exact[0]
    # Matrix display names are deliberately unresolved rather than guessed.
    return None


TABLES = {
    "runs": "run_id run_number event head_sha pr_head_sha pr_number created_at updated_at latest_attempt status conclusion extraction",
    "attempts": "run_id attempt head_sha workflow_execution_sha checkout_sha checkout_coverage source_sha source_id mapping_confidence status conclusion started_at completed_at elapsed_seconds api_updated_at",
    "jobs": "run_id attempt job_id name status conclusion started_at completed_at duration_seconds runner_name labels source_job source_line log_status log_overlong_lines checkout_sha checkout_coverage",
    "steps": "job_id number name status conclusion started_at completed_at duration_seconds source_line mapping",
    "metrics": "job_id step_number metric count total minimum maximum",
    "sources": "source_id revision path url confidence",
}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def save_tables(output, tables):
    for name, fields in TABLES.items():
        with (output / f"{name}.csv").open("w", newline="") as handle:
            writer = csv.DictWriter(handle, fieldnames=fields.split())
            writer.writeheader()
            writer.writerows(tables[name])


def freeze(api, repo, workflow, limit, output):
    manifest_path = output / "manifest.json"
    if manifest_path.exists():
        manifest = json.loads(manifest_path.read_text())
        if manifest["repo"] != repo or manifest["workflow"] != workflow or len(manifest["frozen_runs"]) != limit:
            raise SafeError("manifest_arguments_mismatch")
        return manifest
    frozen = []
    seen = set()
    for run in api.pages(f"repos/{repo}/actions/workflows/{quote(workflow, safe='')}/runs?status=completed", "workflow_runs"):
        if run["id"] in seen:
            continue
        seen.add(run["id"])
        frozen.append(dict(run_id=run["id"], latest_attempt=run["run_attempt"], created_at=run["created_at"]))
        if len(frozen) == limit:
            break
    if len(frozen) != limit:
        raise SafeError("insufficient_completed_runs")
    manifest = dict(schema=SCHEMA, repo=repo, workflow=workflow, frozen_at=utcnow(),
                    frozen_runs=frozen, state="frozen")
    write_json(manifest_path, manifest)
    return manifest


def attempt_jobs(api, repo, run_id, attempt):
    jobs = list(api.pages(f"repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs", "jobs"))
    seen = set()
    for job in jobs:
        if job.get("run_attempt") != attempt or job["id"] in seen:
            raise SafeError("job_attempt_or_identity_mismatch")
        seen.add(job["id"])
    return jobs


def collect(api, manifest, output):
    repo = manifest["repo"]
    tables = {name: [] for name in TABLES}
    source_texts = {}
    source_cache = {}
    run_id = None
    manifest.update(state="collecting", collection_started_at=utcnow(), errors=[])
    write_json(output / "manifest.json", manifest)
    try:
        for frozen in manifest["frozen_runs"]:
            run_id = frozen["run_id"]
            run = api.request(f"repos/{repo}/actions/runs/{run_id}")
            if run["run_attempt"] != frozen["latest_attempt"] or run["status"] != "completed":
                raise SafeError("frozen_run_changed")
            prs = run.get("pull_requests", [])
            row = dict(run_id=run_id, run_number=run["run_number"], event=run["event"],
                       head_sha=run["head_sha"], pr_head_sha=prs[0]["head"]["sha"] if len(prs) == 1 else None,
                       pr_number=prs[0]["number"] if len(prs) == 1 else None,
                       created_at=run["created_at"], updated_at=run["updated_at"],
                       latest_attempt=run["run_attempt"], status=run["status"], conclusion=run["conclusion"], extraction="incomplete")
            tables["runs"].append(row)
            for attempt in range(1, frozen["latest_attempt"] + 1):
                info = api.request(f"repos/{repo}/actions/runs/{run_id}/attempts/{attempt}")
                jobs = attempt_jobs(api, repo, run_id, attempt)
                local_jobs = []
                checkouts = set()
                for job in jobs:
                    parser = LogMetrics(job.get("steps", []))
                    log_status = "not_requested_skipped" if job.get("conclusion") == "skipped" else "available"
                    if log_status == "available":
                        try:
                            api.request(f"repos/{repo}/actions/jobs/{job['id']}/logs", parser)
                        except SafeError as exc:
                            if str(exc) not in {"http_404", "http_410", "response_byte_limit"}:
                                raise
                            log_status = str(exc)
                            parser.reset()  # No partial metrics advertised as complete.
                    checkouts.update(parser.checkouts)
                    tables["metrics"].extend(parser.rows(job["id"]))
                    local_jobs.append((job, parser, log_status))
                checkout = next(iter(checkouts)) if len(checkouts) == 1 else None
                confidence = "checkout_source_not_execution_proof" if checkout else "head_source_unverified"
                revision = checkout or info["head_sha"]
                path = run["path"].split("@")[0]
                cache_key = (revision, path)
                if cache_key not in source_cache:
                    try:
                        blob = api.request(f"repos/{repo}/contents/{path}?ref={revision}")
                        text = base64.b64decode(blob["content"]).decode("utf-8")
                        source_id = hashlib.sha256(text.encode()).hexdigest()[:16]
                        source_texts[source_id] = text
                        source_cache[cache_key] = (source_id, source_jobs(text))
                        tables["sources"].append(dict(source_id=source_id, revision=revision, path=path,
                            url=f"https://github.com/{repo}/blob/{revision}/{path}", confidence="revision_linked_source"))
                    except SafeError as exc:
                        if str(exc) not in {"http_404", "http_410"}:
                            raise
                        source_cache[cache_key] = (None, [])
                source_id, definitions = source_cache[cache_key]
                ends = [j["completed_at"] for j in jobs if stamp(j.get("completed_at")) and j.get("conclusion") != "skipped"]
                end = max(ends) if ends else None
                tables["attempts"].append(dict(run_id=run_id, attempt=attempt, head_sha=info["head_sha"],
                    workflow_execution_sha=None, checkout_sha=checkout,
                    checkout_coverage="unique" if len(checkouts) == 1 else "multiple" if checkouts else "missing",
                    source_sha=revision, source_id=source_id, mapping_confidence=confidence if source_id else "source_missing",
                    status=info["status"], conclusion=info["conclusion"], started_at=info.get("run_started_at"),
                    completed_at=end, elapsed_seconds=duration(info.get("run_started_at"), end), api_updated_at=info.get("updated_at")))
                for job, parser, log_status in local_jobs:
                    definition = map_job(job["name"], definitions)
                    tables["jobs"].append(dict(run_id=run_id, attempt=attempt, job_id=job["id"], name=job["name"],
                        status=job["status"], conclusion=job["conclusion"], started_at=job.get("started_at"), completed_at=job.get("completed_at"),
                        duration_seconds=duration(job.get("started_at"), job.get("completed_at"), job.get("conclusion")),
                        runner_name=job.get("runner_name"), labels=";".join(job.get("labels", [])),
                        source_job=definition["key"] if definition else None, source_line=definition["line"] if definition else None,
                        log_status=log_status, log_overlong_lines=parser.overlong,
                        checkout_sha=next(iter(parser.checkouts)) if len(parser.checkouts) == 1 else None,
                        checkout_coverage="unique" if len(parser.checkouts) == 1 else "multiple" if parser.checkouts else "missing"))
                    last_line = 0
                    for step in job.get("steps", []):
                        matches = [s for s in definition["steps"] if s["name"] == step["name"] and s["line"] > last_line] if definition else []
                        source_line = matches[0]["line"] if len(matches) == 1 else None
                        if source_line:
                            last_line = source_line
                        tables["steps"].append(dict(job_id=job["id"], number=step["number"], name=step["name"],
                            status=step["status"], conclusion=step.get("conclusion"), started_at=step.get("started_at"), completed_at=step.get("completed_at"),
                            duration_seconds=duration(step.get("started_at"), step.get("completed_at"), step.get("conclusion")),
                            source_line=source_line, mapping="named_step_source" if source_line else "generated_action_internal_or_unresolved"))
            row["extraction"] = "complete"
            save_tables(output, tables)
            write_json(output / "workflow_sources.json", source_texts)
            print(f"extracted {len(tables['runs'])}/{len(manifest['frozen_runs'])} runs", flush=True)
        manifest["state"] = "complete"
    except SafeError as exc:
        manifest["state"] = "failed"
        manifest["errors"].append(dict(run_id=run_id, code=str(exc)))
        raise
    except (KeyError, ValueError, TypeError):
        manifest["state"] = "failed"
        manifest["errors"].append(dict(run_id=run_id, code="invalid_response_schema"))
        raise SafeError("invalid_response_schema") from None
    finally:
        save_tables(output, tables)
        write_json(output / "workflow_sources.json", source_texts)
        manifest.update(collection_finished_at=utcnow(), received_bytes=api.bytes, requests=api.requests,
                        counts={key: len(rows) for key, rows in tables.items()},
                        tooling_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
        manifest["files"] = {p.name: dict(bytes=p.stat().st_size, sha256=hashlib.sha256(p.read_bytes()).hexdigest())
                             for p in sorted(output.iterdir()) if p.suffix in {".csv", ".json"} and p.name not in {"manifest.json", "analysis.json", "source_references.json", "execution_index.csv"}}
        write_json(output / "manifest.json", manifest)


def summary(values):
    values = sorted(values)
    return dict(n=len(values), total=round(sum(values), 3), median=statistics.median(values) if values else None,
                p90=values[math.ceil(0.9 * len(values)) - 1] if values else None)


def analyze(output):
    tables = {}
    for name in TABLES:
        with (output / f"{name}.csv").open(newline="") as handle:
            tables[name] = list(csv.DictReader(handle))
    jobs = {j["job_id"]: j for j in tables["jobs"]}
    attempts = {(a["run_id"], a["attempt"]): a for a in tables["attempts"]}
    report = dict(schema=SCHEMA, counts={k: len(v) for k, v in tables.items()},
                  log_coverage=dict(collections.Counter(j["log_status"] for j in jobs.values())),
                  mapping_coverage=dict(collections.Counter(a["mapping_confidence"] for a in attempts.values())),
                  step_mapping_coverage=dict(collections.Counter(s["mapping"] for s in tables["steps"])),
                  conclusions=dict(collections.Counter(r["conclusion"] for r in tables["runs"])),
                  notes=["API execution seconds, not billed cost; p90 is nearest-rank.",
                         "Elapsed ends at last non-skipped job completion, not mutable run updated_at.",
                         "Last-finishing job is an observed terminal job, not a proven DAG critical path.",
                         "Start offset includes dependencies and scheduling; it is not runner queue time.",
                         "Source mappings do not prove workflow execution SHA; action internals unresolved.",
                         "Early/recent halves compare only same source/job/platform/conclusion; workload still varies.",
                         "Cargo times overlap API durations; sccache counters may repeat snapshots, not additive cache rates."])
    from ci_timings_analysis import derive, execution_evidence, workflow_references

    observed_tables = tables
    tables, execution_records = execution_evidence(tables, stamp)
    jobs = {j["job_id"]: j for j in tables["jobs"]}
    report["execution_classification"] = dict(collections.Counter(r["classification"] for r in execution_records))
    report["execution_counts"] = {k: len(v) for k, v in tables.items()}
    report["execution_reconciliation"] = {}
    classes = {r["job_id"]: r["classification"] for r in execution_records}
    for table in ("jobs", "steps"):
        totals = collections.defaultdict(float)
        for row in observed_tables[table]:
            totals[classes[row["job_id"]]] += float(row["duration_seconds"] or 0)
        report["execution_reconciliation"][table] = dict(totals)
    with (output / "execution_index.csv").open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=["job_id", "canonical_job_id", "earliest_observed_attempt", "classification"])
        writer.writeheader()
        writer.writerows(execution_records)
    report["notes"].append("Execution statistics exclude exact same-run cross-attempt carryovers and separately flagged uncertain pre-attempt rows; raw collection counts/coverage remain unchanged.")
    for table, field in (("jobs", "name"), ("steps", "name")):
        grouped = collections.defaultdict(list)
        for row in tables[table]:
            if row["duration_seconds"]:
                grouped[row[field]].append(float(row["duration_seconds"]))
        report[table + "_timings"] = sorted([dict(name=k, **summary(v)) for k, v in grouped.items()], key=lambda x: -x["total"])
    report["attempt_timings"] = []
    cohorts = collections.defaultdict(list)
    for key, attempt in attempts.items():
        selected = [j for j in jobs.values() if (j["run_id"], j["attempt"]) == key and j["duration_seconds"]]
        terminal = max(selected, key=lambda j: j["completed_at"]) if selected else None
        report["attempt_timings"].append(dict(run_id=key[0], attempt=key[1], elapsed_seconds=attempt["elapsed_seconds"] or None,
            job_seconds=round(sum(float(j["duration_seconds"]) for j in selected), 3),
            terminal_job=terminal["name"] if terminal else None,
            start_offsets=[dict(job_id=j["job_id"], seconds=duration(attempt["started_at"], j["started_at"])) for j in selected]))
        for job in selected:
            cohort = (attempt["source_id"], job["source_job"] or job["name"], job["labels"], job["conclusion"])
            cohorts[cohort].append((job["started_at"], float(job["duration_seconds"])))
    report["cohorts"] = []
    for key, values in sorted(cohorts.items()):
        values.sort()
        midpoint = len(values) // 2
        report["cohorts"].append(dict(source_id=key[0], job=key[1], platform=key[2], conclusion=key[3], n=len(values),
            early=summary([v for _, v in values[:midpoint]]), recent=summary([v for _, v in values[midpoint:]])))
    latest = {r["run_id"]: int(r["latest_attempt"]) for r in tables["runs"]}
    report["prior_attempt_job_seconds"] = sum(float(j["duration_seconds"] or 0) for j in jobs.values() if int(j["attempt"]) < latest[j["run_id"]])
    report["cancelled_job_seconds"] = sum(float(j["duration_seconds"] or 0) for j in jobs.values() if j["conclusion"] == "cancelled")
    report["cancelled_run_job_seconds"] = sum(float(j["duration_seconds"] or 0) for j in jobs.values() if attempts[(j["run_id"], j["attempt"])]["conclusion"] == "cancelled")
    cancelled_runs = {r["run_id"] for r in tables["runs"] if r["conclusion"] == "cancelled"}
    report["cancelled_final_run_job_seconds"] = sum(float(j["duration_seconds"] or 0) for j in jobs.values() if j["run_id"] in cancelled_runs)
    report["failed_attempt_job_seconds"] = sum(float(j["duration_seconds"] or 0) for j in jobs.values() if attempts[(j["run_id"], j["attempt"])]["conclusion"] == "failure")
    report["metric_coverage"] = dict(collections.Counter(m["metric"] for m in tables["metrics"]))
    manifest = json.loads((output / "manifest.json").read_text())
    texts = json.loads((output / "workflow_sources.json").read_text())
    references = workflow_references(texts, source_jobs)
    write_json(output / "source_references.json", references)
    report["extended"] = derive(dict(tables, observed_jobs=observed_tables["jobs"], frozen_runs=manifest["frozen_runs"]), references, summary, duration)
    report["analysis_version"] = 2
    report["analysis_code_sha256"] = {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
        for p in (Path(__file__), Path(__file__).with_name("ci_timings_analysis.py"))}
    report["dataset_bytes_before_analysis"] = sum((output / name).stat().st_size
        for name in ["manifest.json", "workflow_sources.json"] + [name + ".csv" for name in TABLES])
    write_json(output / "analysis.json", report)
    print(json.dumps({k: report[k] for k in ("counts", "log_coverage", "mapping_coverage")}, sort_keys=True))
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("run", "collect"):
        command = sub.add_parser(name)
        command.add_argument("--repo", default="dekopon-agents/dekopon")
        command.add_argument("--output", type=Path, required=True)
        if name == "run":
            command.add_argument("run")
        else:
            command.add_argument("--workflow", default="ci.yml")
            command.add_argument("--limit", type=int, default=50)
            command.add_argument("--freeze-only", action="store_true")
    sub.add_parser("analyze").add_argument("dataset", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "analyze":
            analyze(args.dataset)
            return 0
        if not re.fullmatch(r"[\w.-]+/[\w.-]+", args.repo):
            raise SafeError("invalid_repository")
        args.output.mkdir(parents=True, exist_ok=True)
        api = Gh()
        if args.command == "collect":
            if not 1 <= args.limit <= 1000:
                raise SafeError("invalid_limit")
            manifest = freeze(api, args.repo, args.workflow, args.limit, args.output)
            if args.freeze_only:
                print(f"frozen {len(manifest['frozen_runs'])} completed runs")
                return 0
        else:
            repo, run_id = run_identity(args.run, args.repo)
            info = api.request(f"repos/{repo}/actions/runs/{run_id}")
            if info["status"] != "completed":
                raise SafeError("run_not_completed")
            manifest = dict(schema=SCHEMA, repo=repo, workflow=info["path"], frozen_at=utcnow(),
                            frozen_runs=[dict(run_id=run_id, latest_attempt=info["run_attempt"], created_at=info["created_at"])])
        collect(api, manifest, args.output)
        analyze(args.output)
        return 0
    except SafeError as exc:
        print(f"ci_timings: {exc}", file=sys.stderr)
        return 1
    except (OSError, ValueError, KeyError, TypeError):
        # JSON/API values, paths and subprocess arguments must not escape in tracebacks.
        print("ci_timings: local_io_or_schema_error", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
