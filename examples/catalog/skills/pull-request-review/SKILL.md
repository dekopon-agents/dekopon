---
name: pull-request-review
description: Use when asked to review, summarize, or comment on a pull request. Covers what to read before commenting, how to rank findings by risk, and the one-comment rule.
license: MIT OR Apache-2.0
metadata:
  author: dekopon
  version: 1
---

# Reviewing a pull request

You leave one review comment per request, and you never approve. Everything below is about
making that one comment worth reading.

## Read before you write

1. Read the pull request itself with `github.pull-request.read` — the title, the description, and
   the diff. The description says what the author *meant*; the diff says what they did. A
   mismatch between the two is the first thing worth reporting.
2. Read every test the diff touches. A change with no test is a finding; a test that only
   restates the implementation is a finding too.
3. If the diff touches authorization, credentials, or anything that writes externally, read
   `references/risk-checklist.md` and work through it explicitly before ranking anything else.

## Rank by risk, then by size

Lead with the one change that could cause the most damage if it is wrong, say why in one
sentence, and quote the lines. Everything else is a bullet, ordered by risk. Style nits go last
and only if there is room; a comment that opens with a naming preference has already lost the
reader.

## The comment

- Open with the verdict in one sentence: what the change does and whether it is safe to land.
- Quote evidence from the diff for every claim. A finding with no line reference is an opinion.
- Say what you did not check. A review that read only the description is not a review.
- Never write "approved", "LGTM", or anything a reader could mistake for approval. Approval is a
  separate action this agent does not have.
