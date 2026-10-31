---
name: succinct-docs
description: Write or edit Dekopon documentation (Markdown under docs/, crates/, examples/, the root README) so the text that survives the cut is clear. First apply AGENTS.md "When to document" to decide whether text should exist, then these clarity rules from NASA TM-105419 to decide how it reads. Use for any doc or README change, and when reviewing one.
---

# Succinct docs

Two passes, in this order:

1. **Cut.** [AGENTS.md#documentation](../../../AGENTS.md#documentation) decides whether text exists:
   "When to document" and rules 1–8. Never polish a paragraph that pass would delete.
2. **Clarity.** What survives follows the rules below, from Carol A. Vidoli, *Technical Report
   Writing*, NASA TM-105419, 1992 (<https://ntrs.nasa.gov/citations/19930013813>).

## Clarity rules

1. **Purpose first.** The opening says what the reader ends with, then the order of what follows.
   ("State your purpose or objective clearly and follow it with a concise description of the
   method you will use")
2. **Get to the point.** Every sentence is a step, a reason or a limit; keep one sentence of reason
   where a restriction looks arbitrary. ("Technical reports are not mystery novels")
3. **Concise, not condensed.** Cut words, never a contract the reader needs. ("brief but
   comprehensible")
4. **Exact.** Every command, path, owner, mode, key and expected result is exact; no step depends
   on a guess.
5. **One fact, one place.** State it once and link the heading that owns it. ("never in more than
   one way")
6. **Natural words.** Short familiar words, no misleading jargon. ("Write to express, not to
   impress.")
7. **Major ideas first.** Each section opens with its action or rule; detail follows.
8. **No rough-draft faults.** No noun stacks or prepositional chains, no abstract noun where a verb
   works, parallel lists, nothing more complicated than needed.
9. **Headings.** Equal-weight headings share a form; at least two subheadings or none; at most
   three levels.
10. **Objective.** State each limit beside the claim it qualifies; no hedging, no selling, nothing
    untested presented as tested.

## Never lose in a clarity edit

- A security or configuration contract the cut kept: owners, modes, where credentials live and
  never go, authority limits, spending limits.
- A code or config block: it stays byte-identical unless it is wrong.
- A heading another file links to, including the website (`dekopon-agents.github.io/src`).

## Failure modes seen in review

- **A qualified claim becomes universal**, or **"only" sneaks in.**
- **A condition drops out:** "rotate with both daemons running" lost "while the gateway is
  connected".
- **A softer word becomes a stronger one:** "limit" became "cap" for a budget that can overshoot.
- **Carried-over text is wrong:** check every claim a section keeps, not only the ones you change.
- **A rule restates a fact and rots:** link the doc that owns the fact instead.
- **A deletion leaves a dangling pointer:** "see X" with no X.

## Review

A reviewer who did not write the change checks each kept claim against the code and tags each
finding `blocking` (a lost contract, a false claim, a step the reader cannot follow) or `follow-up`.
