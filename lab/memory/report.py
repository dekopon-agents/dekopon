#!/usr/bin/env python3
import collections
import csv
import json
import math
import re
import sys
from pathlib import Path

LABELS = {name: "required" for name in (
    "baseline", "flood", "summarizer-hang", "summarizer-fail",
    "summarizer-refuse", "unloadable-journal")}
LABELS.update({name: "exploratory" for name in (
    "restart-mid-summary", "summaries-off-vs-on", "config-check")})
DEADLINES = {name: seconds for name, seconds in (
    ("baseline", 1800), ("flood", 1800), ("summarizer-hang", 1800),
    ("summarizer-fail", 1200), ("summarizer-refuse", 1200),
    ("unloadable-journal", 1200), ("restart-mid-summary", 1200),
    ("summaries-off-vs-on", 1800), ("config-check", 900))}


SUMMARIZER_BEHAVIOURS = {
    "summarizer-hang": ("hang", {"hung-client-gone", "hung-until-shutdown"}),
    "summarizer-fail": ("fail", {"503"}),
    "summarizer-refuse": ("refuse", {"refused"}),
}


def assertion_requirements(scenario):
    names = ["every turn got a reply"]
    if scenario in SUMMARIZER_BEHAVIOURS:
        names += ["configured summarizer behavior was exercised", "no reply waited on a summary",
                  "other routes did not wait on a summary"]
    if scenario == "restart-mid-summary":
        names += ["slot resume"]
    if scenario == "unloadable-journal":
        names += ["exactly one reset record", "clean start"]
    return dict.fromkeys(names, LABELS[scenario] == "required")


def summarizer_evidence(meta, turns, summaries):
    routes = {route: [t for t in turns if t["conversation"] == route] for route in ("summary", "none")}
    for route, rows in routes.items():
        if len(rows) != 10 or {row["turn"] for row in rows} != {str(n) for n in range(1, 11)}:
            raise ValueError(f"incomplete {route} route evidence")
    numbers = {"summary_turns": len(routes["summary"]), "other_turns": len(routes["none"])}
    applicability = (meta.get("findings") or {}).get("summarizer applicability")
    if applicability is not None:
        if (not isinstance(applicability, dict) or applicability.get("result") != "not-applicable"
                or not isinstance(applicability.get("reason"), str) or not applicability["reason"].strip()
                or summaries):
            raise ValueError("invalid summarizer applicability evidence")
        return "not-applicable", dict(numbers, calls=0)
    behaviour, statuses = SUMMARIZER_BEHAVIOURS[meta["scenario"]]
    matching = [call for call in summaries
                if call.get("tag", {}).get("sum") == behaviour and call.get("status") in statuses
                and call.get("tag", {}).get("turn") in {f"summary-{n}" for n in range(1, 11)}]
    if not matching:
        raise ValueError("configured summarizer behavior was not observed")
    return "pass", dict(numbers, calls=len(matching))


def evidence(run):
    meta = json.loads((run / "meta.json").read_text())
    if meta["scenario"] not in LABELS or not isinstance(meta["sha"], str):
        raise ValueError("invalid scenario or SHA")
    if not math.isfinite(meta["summarizer_timeout_ms"]) or meta["summarizer_timeout_ms"] <= 0:
        raise ValueError("invalid summarizer timeout")
    with (run / "latency.csv").open(newline="") as stream:
        reader = csv.DictReader(stream, strict=True)
        required = {"kind", "wall_ms", "conversation", "phase", "summary_in_flight"}
        if not required <= set(reader.fieldnames or []):
            raise ValueError("invalid latency columns")
        turns = list(reader)
    if not turns:
        raise ValueError("no turns ran")
    for turn in turns:
        if None in turn or any(value is None for value in turn.values()):
            raise ValueError("invalid latency row")
        wall = float(turn["wall_ms"])
        if not math.isfinite(wall) or wall < 0 or not turn["kind"] or not turn["conversation"]:
            raise ValueError("invalid latency value")
    batches = 0
    with (run / "traces.jsonl").open() as stream:
        for line in stream:
            if not isinstance(json.loads(line), dict):
                raise ValueError("invalid trace evidence")
            batches += 1
    if not batches:
        raise ValueError("invalid trace evidence")
    if meta["scenario"] in SUMMARIZER_BEHAVIOURS:
        summaries = [call for call in records(run / "model-calls.jsonl", strict=True)
                     if call.get("kind") == "summary"]
        summarizer_evidence(meta, turns, summaries)
    return meta, turns


SLOT = re.compile(r"slot", re.I)
DROPPED = re.compile(r"drop", re.I)


def records(path, *, strict=False):
    if not path.exists() and not strict:
        return
    with path.open(errors="strict" if strict else "replace") as stream:
        for line in stream:
            try:
                row = json.loads(line)
            except ValueError:
                if strict:
                    raise
                continue
            if isinstance(row, dict):
                yield row
            elif strict:
                raise ValueError("invalid record")


def event_name(row):
    fields = row.get("fields") if isinstance(row.get("fields"), dict) else {}
    return row.get("event") or fields.get("event") or row.get("audit.event")


def percentile(values, fraction):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, round(fraction * (len(ordered) - 1))))]


def latency_line(label, values):
    if not values:
        return f"| {label} | 0 | – | – | – |"
    return (f"| {label} | {len(values)} | {percentile(values, 0.5):.0f} | {percentile(values, 0.95):.0f} "
            f"| {max(values):.0f} |")


def spans(path):
    services = collections.Counter()
    names = collections.Counter()
    parents = {}
    for batch in records(path):
        for resource in batch.get("resourceSpans", []):
            service = next((a["value"].get("stringValue") for a in resource.get("resource", {})
                            .get("attributes", []) if a.get("key") == "service.name"), "?")
            for scope in resource.get("scopeSpans", []):
                for span in scope.get("spans", []):
                    services[service] += 1
                    names[span.get("name")] += 1
                    parents[span.get("spanId")] = span.get("parentSpanId") or None
    roots = sum(1 for parent in parents.values() if parent is None)
    orphans = sum(1 for parent in parents.values() if parent is not None and parent not in parents)
    return services, names, (len(parents), roots, orphans)


def log_records(path):
    services = collections.Counter()
    for batch in records(path):
        for resource in batch.get("resourceLogs", []):
            service = next((a["value"].get("stringValue") for a in resource.get("resource", {})
                            .get("attributes", []) if a.get("key") == "service.name"), "?")
            for scope in resource.get("scopeLogs", []):
                services[service] += len(scope.get("logRecords", []))
    return services


def tree_bytes(path):
    if not path.exists():
        return 0, 0
    files = [item for item in path.rglob("*") if item.is_file()]
    return sum(item.stat().st_size for item in files), len(files)


def build(run):
    meta, turns = evidence(run)
    ref8 = meta["sha"][:8]
    gateway = list(records(run / "gatewayd.stdout.log"))
    broker = list(records(run / "brokerd.stdout.log"))
    events = collections.Counter(event_name(row) for row in gateway if event_name(row))
    calls = list(records(run / "model-calls.jsonl"))
    summaries = [c for c in calls if c.get("kind") == "summary"]
    chats = [c for c in calls if c.get("kind") == "chat"]
    lab_events = records(run / "events.jsonl")
    out = [f"# {meta['scenario']} at {ref8}", ""]
    out += [f"- ref: `{meta['ref']}` → `{meta['sha']}`", f"- profile: {meta['profile']} ({meta['build']} build)",
            f"- uids: brokerd {meta['uids']['broker']}, gatewayd {meta['uids']['gateway']}"
            f" ({meta['uids']['separation']})",
            "- lab overlay: " + (f"**{meta['overlay']['name']}** (patch sha256 `{meta['overlay']['patch_sha256']}`, "
                                 f"CA sha256 `{meta['overlay']['ca_sha256']}`) — not stock code"
                                 if meta.get("overlay") else "none (stock code)"),
            f"- started {meta['started_utc']}, finished {meta.get('finished_utc', '?')}",
            f"- status: **{meta.get('status', 'unknown')}**", ""]
    if meta.get("cost"):
        out += [f"- real-profile cost: ${meta['cost']['spent_usd']} of ${meta['cost']['cap_usd']} "
                f"({meta['cost']['prompt_tokens']} prompt, {meta['cost']['completion_tokens']} completion tokens)", ""]
    replies = [t for t in turns if t["kind"] == "reply"]
    kinds = collections.Counter(t["kind"] for t in turns)
    slot_events = {name: count for name, count in events.items() if SLOT.search(name)}
    dropped = {name: count for name, count in events.items() if DROPPED.search(name)}
    resets = events.get("gateway_journal_reset", 0)
    restarts = sum(1 for e in lab_events if e.get("event") == "daemon-ready" and e.get("daemon") == "gatewayd") - 1
    out += ["## Counts", "",
            f"- turns sent: {len(turns)}; replies: {len(replies)}; other outcomes: "
            f"{dict((k, v) for k, v in kinds.items() if k != 'reply') or 'none'}",
            "- summary slot transitions: " + (f"{sum(slot_events.values())} {slot_events}" if slot_events
                                              else f"0 (no slot events at {ref8})"),
            "- dropped jobs: " + (f"{sum(dropped.values())} {dropped}" if dropped
                                  else f"0 (no drop events at {ref8})"),
            f"- journal resets (`gateway_journal_reset`): {resets}",
            f"- gatewayd restarts: {max(restarts, 0)}",
            f"- model calls: {len(chats)} chat, {len(summaries)} summarizer "
            f"{dict(collections.Counter(c.get('status') for c in summaries))}",
            "- memory deviations: " + str(dict(collections.Counter(
                (row.get("fields", row)).get("reason") for row in gateway
                if event_name(row) == "gateway_memory_deviation")) or "none"),
            f"- journal append failures: {events.get('gateway_journal_append_failed', 0)}; "
            f"retention failures: {events.get('gateway_memory_retention_failed', 0)}",
            f"- gatewayd errors/warnings: {sum(1 for r in gateway if r.get('level') == 'ERROR')}/"
            f"{sum(1 for r in gateway if r.get('level') == 'WARN')}; "
            f"brokerd: {sum(1 for r in broker if r.get('level') == 'ERROR')}/"
            f"{sum(1 for r in broker if r.get('level') == 'WARN')}", ""]
    wall = lambda rows: [float(t["wall_ms"]) for t in rows]  # noqa: E731
    out += ["## Reply latency (ms, replies only)", "", "| set | n | p50 | p95 | max |", "|---|---|---|---|---|",
            latency_line("all", wall(replies)),
            latency_line("summary in flight", wall([t for t in replies if t["summary_in_flight"] == "yes"])),
            latency_line("no summary in flight", wall([t for t in replies if t["summary_in_flight"] == "no"]))]
    for key in sorted({(t["phase"], t["conversation"]) for t in replies}):
        out.append(latency_line(f"{key[0]} / {key[1]}",
                                wall([t for t in replies if (t["phase"], t["conversation"]) == key])))
    out += [""]
    out += ["## Resources", ""]
    peaks = {}
    if (run / "rss.csv").exists():
        for row in csv.DictReader(open(run / "rss.csv", encoding="utf-8")):
            peak = peaks.setdefault(row["daemon"], [0, 0, 0])
            peak[0] = max(peak[0], int(row["rss_kib"]))
            peak[1] = max(peak[1], int(row["hwm_kib"]))
            peak[2] += 1
    for daemon, (rss, hwm, samples) in sorted(peaks.items()):
        out.append(f"- {daemon}: peak sampled RSS {rss / 1024:.1f} MiB, peak VmHWM {hwm / 1024:.1f} MiB "
                   f"({samples} samples)")
    journal_bytes, journal_files = tree_bytes(run / "journal")
    out.append(f"- journal at end: {journal_bytes} bytes in {journal_files} files")
    for label in ("journal-at-kill", "journal-before-corrupt", "journal-off"):
        if (run / label).exists():
            size, count = tree_bytes(run / label)
            out.append(f"- {label}: {size} bytes in {count} files")
    services, names, (unique, roots, orphans) = spans(run / "traces.jsonl")
    out.append(f"- spans exported: {dict(services) or 'none'}; log records: {dict(log_records(run / 'logs.jsonl')) or 'none'}")
    out.append(f"- span parentage: {unique} unique spans, {roots} roots, {orphans} whose parent was never exported")
    if names:
        out.append("- top span names: " + ", ".join(f"{n} {c}" for n, c in names.most_common(8)))
    out += [""]
    lines, assertions = verdicts(meta, turns, replies, summaries, events, slot_events, resets, ref8)
    out += lines
    label = LABELS[meta["scenario"]]
    required = [a for a in assertions if a["required"]]
    if label == "exploratory":
        outcome, reason = "exploratory", "exploratory: not a regression assertion"
        out += [reason, ""]
    elif any(a["result"] == "fail" for a in required):
        outcome = "fail"
        reason = "; ".join(a["name"] for a in required if a["result"] == "fail")
    elif any(a["result"] == "not-applicable" for a in required):
        outcome, reason = "not-applicable", f"required assertion not applicable at {ref8}"
    elif not required:
        raise ValueError("no required assertions")
    else:
        outcome, reason = "pass", "every required assertion passed"
    if meta.get("diagnostic"):
        out += ["diagnostic: verdict not enforced", ""]
    verdict = {"scenario": meta["scenario"], "label": label, "outcome": outcome,
               "reason": reason, "assertions": assertions, "diagnostic": bool(meta.get("diagnostic"))}
    findings = meta.get("findings") or {}
    if findings:
        out += ["## Findings", ""] + [f"- {key}: {value}" for key, value in findings.items()] + [""]
    return "\n".join(out), verdict


def verdicts(meta, turns, replies, summaries, events, slot_events, resets, ref8):
    scenario = meta["scenario"]
    timeout = meta["summarizer_timeout_ms"]
    lines = ["## Verdict", ""]
    assertions = []
    requirements = assertion_requirements(scenario)

    def assertion(name, passed, **numbers):
        result = "not-applicable" if passed is None else "pass" if passed else "fail"
        assertions.append({"name": name, "result": result,
                           "numbers": numbers, "required": requirements[name]})
    lost = [t for t in turns if t["kind"] != "reply"]
    if not turns:
        raise ValueError("no turns ran")
    assertion("every turn got a reply", not lost, turns=len(turns), lost=len(lost))
    lines.append(f"- every turn got a reply: {'yes' if not lost else 'NO, ' + str(len(lost)) + ' did not'}")
    if scenario in SUMMARIZER_BEHAVIOURS:
        exercised, numbers = summarizer_evidence(meta, turns, summaries)
        assertion("configured summarizer behavior was exercised", True if exercised == "pass" else None, **numbers)
        if exercised == "not-applicable":
            reason = meta["findings"]["summarizer applicability"]["reason"]
            lines.append(f"- summarizer assertions: **not applicable at {ref8}** ({reason})")
            assertion("no reply waited on a summary", None)
            assertion("other routes did not wait on a summary", None)
        else:
            waited = [t for t in replies if t["conversation"] == "summary" and float(t["wall_ms"]) >= timeout * 0.8]
            lines.append(f"- summary-route replies that took ≥ 80% of the {timeout} ms summarizer timeout: "
                         f"{len(waited)} of {sum(1 for t in replies if t['conversation'] == 'summary')}"
                         + (" — **replies waited on a summary**" if waited else " — no reply waited on a summary"))
            assertion("no reply waited on a summary", not waited,
                      slow=len(waited), threshold_ms=timeout * 0.8,
                      replies=sum(t["conversation"] == "summary" for t in replies))
            other = [t for t in replies if t["conversation"] != "summary" and float(t["wall_ms"]) >= timeout * 0.8]
            lines.append(f"- other-route replies that slow: {len(other)}")
            assertion("other routes did not wait on a summary", not other, slow=len(other), threshold_ms=timeout * 0.8)
    if scenario == "restart-mid-summary":
        findings = meta.get("findings", {})
        if not slot_events:
            assertions.append({"name": "slot resume", "result": "not-applicable", "numbers": {},
                               "required": requirements["slot resume"]})
            lines.append(f"- slot resume: **not applicable at {ref8}** (no summary slot events or files; "
                         f"the summarizer was re-called {findings.get('summarizer re-called for killed turn after restart')} "
                         "times for the killed turn)")
        else:
            resumed = findings.get("summarizer re-called for killed turn after restart", 0)
            assertion("slot resume", bool(resumed), resumed=resumed)
            lines.append(f"- slot resume: {'pass' if resumed else 'FAIL'} (summarizer re-called {resumed} times)")
    if scenario == "unloadable-journal":
        assertion("exactly one reset record", resets == 1, resets=resets)
        lines.append(f"- exactly one reset record: {'pass' if resets == 1 else 'FAIL (' + str(resets) + ')'}")
        after = [t for t in turns if t["phase"] == "after"]
        assertion("clean start", bool(after) and all(t["kind"] == "reply" for t in after),
                  turns=len(after), replies=sum(t["kind"] == "reply" for t in after))
        lines.append(f"- clean start: {'pass' if after and all(t['kind'] == 'reply' for t in after) else 'FAIL'}"
                     f" ({sum(1 for t in after if t['kind'] == 'reply')}/{len(after)} replies after boot)")
    if scenario == "flood":
        lines.append("- tool output size: see Findings (the shell caps a script's output)")
    return lines + [""], assertions


def main():
    run = Path(sys.argv[1])
    text, verdict = build(run)
    (run / "summary.md").write_text(text + "\n")
    (run / "verdict.json").write_text(json.dumps(verdict, indent=2) + "\n")
    print(text)


if __name__ == "__main__":
    main()
