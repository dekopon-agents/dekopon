---
name: succinct-docs
description: Write or rewrite Dekopon documentation (Markdown under docs/, examples/, crates/, the root README) as brief, authoritative text for skilled engineers and coding agents. Scores a document against a rubric taken from NASA TM-105419 before and after editing, and keeps every contract. Use when writing a new doc, rewriting or shortening an existing one, or reviewing a doc change.
---

# Succinct docs

Readers are skilled engineers and coding agents. They know Unix, containers, Kubernetes, YAML,
Slack and GitHub. They do not know Dekopon. Give them the shortest text that gets them running
correctly. Fewer tokens is a goal; a lost contract is a defect.

## Dekopon rules

- **No history.** Dekopon is pre-1.0. Backwards compatibility is not a goal, and no doc spends
  words on it. Describe the current release only.
  - No "previously", no "migrating from", no deprecation notes and no compatibility tables.
  - An old configuration fails loudly; the doc names the current key, not the old one.
  - Delete sections that exist only for the past.
- **Code proves what exists.** Verify every claim you change against the code, the CI workflow
  or a released binary before you keep it. Mark anything unverified, never present it as tested.
- **Follow [AGENTS.md](../../../AGENTS.md).** Keep CI-gate sentences accurate, keep every
  `audit.event` name that `docs/observability.md` must list, and never edit `CHANGELOG.md`.

## Rubric

The source is Carol A. Vidoli, *Technical Report Writing*, NASA TM-105419, NASA Lewis Research
Center, 1992 (<https://ntrs.nasa.gov/citations/19930013813>). Score each criterion 1–5, where 5
means no violation is left (maximum 50). Score before and after editing, and cite one line for
every score below 5.

1. **Limiting sentence.** The opening says what the reader ends with and who the doc is for, then
   gives the step order or a table of contents. The body follows that plan. ("a single sentence
   that states the subject, scope, and purpose")
2. **Get to the point.** Every sentence is a step or a contract the reader must honour. Rationale,
   alternatives the reader will not take and asides go, or become a link. ("Technical reports
   are not mystery novels")
3. **Concise, not condensed.** Cut verification narration, receipts and "we tested" prose, but
   never below the floor. ("brief but comprehensible")
4. **Clear.** Every command, path, owner, mode, key and expected result is exact. No step depends
   on a guess.
5. **One fact, one place.** A fact appears once per doc; across docs, the owner states it and the
   rest link to it. ("never in more than one way")
6. **Natural words.** Short familiar words, no jargon that can mislead, no "Official Style".
   ("Write to express, not to impress.")
7. **Major ideas first.** Each section opens with its action or rule; detail follows in bullets.
8. **No rough-draft faults.** No noun stacks or prepositional chains, no abstract nouns where a
   verb works, parallel lists, no phrasing more complex than needed, no unnecessary words.
9. **Headings.** Equal-weight headings share a form; at least two subheadings or none; at most
   three levels; Dekopon terms defined once.
10. **Objective.** Authoritative voice; each limit stated once beside the claim it qualifies; no
    hedging or selling.

## Floor: never cut

- Security and configuration contracts: who owns each file, mode and directory; where
  credentials live and where they never go; authority limits (Cedar grants, capabilities, what an
  optional write grant does not grant); spending and resource limits.
- Config keys, chart values, defaults, mounts and wire names.
- Failure symptoms and the log or event that shows them.
- Upgrade and rollback steps for the current release.
- What CI proves and what it does not.
- Every heading another file links to. Before renaming one, grep this repository and the website
  repository (`dekopon-agents.github.io/src`) for `<file>#anchor` and for in-file `(#anchor)` links.

## Procedure

1. **Map the doc.** List its headings and every inbound link to them. Read the owning docs that
   it restates.
2. **Score it** against the rubric.
3. **Rewrite.**
   - Open with the limiting sentence and the plan.
   - Make each section an action followed by its contract; turn paragraphs into steps, bullets
     or a table.
   - Replace restated facts with a link to the precise sub-heading that owns them. Link only when
     the owner already states the fact.
   - Code and config blocks stay byte-identical unless one is wrong. Compare them with
     `git show HEAD:<file>` before finishing. A wrong block is a separate, verified fix.
4. **Check every changed claim** (see Failure modes).
5. **Run the gates:** `git diff --check`,
   `python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md`,
   and a link check of every relative link and anchor you touched.
6. **Score again**, then report:
   - scores before and after, and words before and after;
   - each fact removed rather than reworded, with where it is still stated or why it was dropped;
   - headings renamed or removed, and what links to them now;
   - gate results as observed;
   - `Choices I made`: each place you read the brief's intent over its text.
7. **Review.** A fresh reviewer who did not write the change reads the diff against the floor and
   tags each finding `blocking` (a lost contract, a false claim, a step the reader can no longer
   follow) or `follow-up`. Fix blocking findings before merge.

## Failure modes seen in rewrites

Each of these shipped in a draft and was caught only by review. Check for them explicitly.

- **A qualified claim becomes universal.** "check accepts the sample IDs" became "check accepts
  every placeholder"; the binary refuses some.
- **A condition drops out.** "Rotate with both daemons running" lost its condition, but the reload
  only happens while the gateway is connected.
- **A fact moves into the wrong context.** A log path true under the service manager was attached
  to the foreground run, where output goes to the terminal.
- **Moving a block changes what a link lands on.** A block moved under a heading that another doc
  links to, so readers following that link got instructions the linking doc forbids.
- **A softer word becomes a stronger one.** "limit" became "cap" for a budget that can overshoot.
- **"Only" sneaks in.** A file that "holds the three lines" became one that "holds only" them,
  stricter than the code.
- **Links land on the parent section** instead of the sub-heading that states the fact.
- **Headings forced into one form** read awkwardly or stop matching a table of contents; prefer
  the clearer heading.
- **Concurrent editors drift.** Two editors on overlapping docs each assume the other keeps a
  fact. Rewrite one doc at a time.
