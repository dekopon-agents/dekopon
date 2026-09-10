# Risk checklist for sensitive diffs

Work through every line. Report each one you cannot answer "yes" to.

- Does every new external write have an explicit, narrowly named capability behind it?
- Does a read path anywhere now imply a write path?
- Is any credential, token, or secret value written to a log, a prompt, a comment, or a test?
- Does the change trust text from a repository, an issue, or a fetched page as an instruction?
- Is every error's cause preserved, or does a `map_err(|_| ...)` discard one?
- Does anything that grows — a buffer, a retry loop, a queue — have a bound and an owner?
