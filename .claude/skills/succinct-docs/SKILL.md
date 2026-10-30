---
name: succinct-docs
description: Write or rewrite Dekopon documentation (Markdown under docs/, examples/, crates/, the root README) as brief, authoritative text for skilled engineers and coding agents. Scores a document against a rubric taken from NASA TM-105419 before and after editing, and keeps every contract. Use when writing a new doc, rewriting or shortening an existing one, or reviewing a doc change.
---

# Succinct docs

Readers are skilled engineers and coding agents. They know Unix, containers, Kubernetes, YAML,
Slack and GitHub. They do not know Dekopon. Give them the shortest text that gets them running
correctly. Fewer tokens is a goal; a lost contract is a defect.

The hard rules are in [AGENTS.md#documentation](../../../AGENTS.md#documentation). This skill is
how to meet them.

## Dekopon rules

- **Current tree only.** Dekopon is pre-1.0; backwards compatibility is not a goal.
  - Describe what the code does now. Future work keeps its Committed direction or Exploration label.
  - No history, "previously", migration steps, version minimums or compatibility guidance. Delete
    sections that exist only for the past.
  - `CHANGELOG.md` is the only history; [upgrading.md](../../../docs/upgrading.md) holds only the
    current release's upgrade and rollback steps.
  - Name only the current key. Do not promise that an old key fails: some inputs, such as the
    gateway's env file, ignore unknown keys.
- **No receipts.** No CI run IDs, OS or Homebrew patch versions, call counts or "we tested".
- **Code proves what exists.** Verify every claim you change against the code, the CI workflow or a
  released binary, and record the `file:line` you checked. Never present an unverified claim as
  tested.
- **Stop for the owner** when a doc and the code disagree, or a cut would change a contract
  (`AGENTS.md`, "Stop for the owner").

## Rubric

The source is Carol A. Vidoli, *Technical Report Writing*, NASA TM-105419, NASA Lewis Research
Center, 1992 (<https://ntrs.nasa.gov/citations/19930013813>). Score each criterion 1–5, where 5
means no violation is left (maximum 50). Score before and after editing, and cite one line for
every score below 5.

1. **Purpose first.** The opening says what the reader ends with and who the doc is for, then gives
   the step order or a table of contents. The body follows that plan. ("State your purpose or
   objective clearly and follow it with a concise description of the method you will use")
2. **Get to the point.** Every sentence is a step or a contract the reader must honour. Rationale,
   alternatives the reader will not take and asides go, or become a link. Keep one sentence of
   reason where a restriction looks arbitrary (often a security reason) or the obvious alternative
   is wrong. ("Technical reports are not mystery novels")
3. **Concise, not condensed.** Cut verification narration and receipts, but never below the floor.
   ("brief but comprehensible")
4. **Clear.** Every command, path, owner, mode, key and expected result is exact. No step depends
   on a guess.
5. **One fact, one place.** A fact appears once per doc. Across docs, the owner in the
   [area index](../../../docs/README.md#change-a-specific-area) states it and the rest link to the
   precise heading. ("never in more than one way")
6. **Natural words.** Short familiar words, no jargon that can mislead, nothing stilted.
   ("Write to express, not to impress.")
7. **Major ideas first.** Each section opens with its action or rule; detail follows in bullets.
8. **No rough-draft faults.** No noun stacks or prepositional chains, no abstract nouns where a
   verb works, parallel lists, no phrasing more complex than needed, no unnecessary words.
9. **Headings.** Equal-weight headings share a form unless that makes one unclear; at least two
   subheadings or none; at most three levels; Dekopon terms defined once.
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
- Every heading another file links to, including links from `CHANGELOG.md` and from the website
  repository (`dekopon-agents/dekopon-agents.github.io`, `src/`). Fetch the website repository to
  grep it, or name that check as not run.
- Text that gates or packaging read: every backticked `audit.event` name in `observability.md`;
  READMEs that ship in release archives (root, brokerd, gatewayd, console) must read on their own.

## Procedure

1. **Map the doc.** List its headings and every inbound link to them. Read the owning docs it
   restates.
2. **Score it** against the rubric.
3. **Rewrite.**
   - Open with the purpose and the plan.
   - Make each section an action followed by its contract; turn paragraphs into steps, bullets
     or a table.
   - Replace restated facts with a link to the precise heading that owns them. Link only when the
     owner already states the fact.
   - Code and config blocks stay byte-identical unless one is wrong. Compare them with
     `git show HEAD:<file>` before finishing. A wrong block is a separate, verified fix.
4. **Check every changed claim** against its source and against the failure modes below.
5. **Run the gates** in [development.md#documentation-gates](../../../docs/development.md#documentation-gates)
   and `git diff --check`. The repository has no link checker: check every relative link and anchor
   you touched with a short script over the target's headings using GitHub's slug rules.
6. **Score again**, then report:
   - scores and words, before and after;
   - each fact removed rather than reworded, with where it is still stated or why it was dropped;
   - headings renamed or removed, and what links to them now;
   - gate results as observed;
   - `Choices I made`: each place you read the brief's intent over its text.
7. **Review.** A reviewer who did not write the change (a human, or under an owner-approved plan a
   fresh agent) re-checks claims against the sources, not the editor's report. Each finding is
   `blocking` (a lost contract, a false claim, a step the reader can no longer follow) or
   `follow-up`. Fix blocking findings before merge.

## Failure modes seen in rewrites

Each of these shipped in a draft and was caught only by review.

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
- **Concurrent editors drift.** Two editors on docs that share facts each assume the other keeps a
  fact. Rewrite one doc at a time; split a very large doc into disjoint section ranges with one
  reviewer for the whole.
- **History creeps back.** Another session re-added a migration section and CI run receipts days
  after a pass removed them. The rules live in `AGENTS.md` so every session sees them.
