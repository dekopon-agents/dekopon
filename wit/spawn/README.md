# dekopon:spawn@0.1.0

`spawn.wit` defines the `run` interface for child gateway shell scripts. The
broker host imports it in its private provider world; the SDK imports it in
`spawn-client`. The published `provider@0.4.0` world does not change.

`run(script, stdin)` returns one stdout reader and one status resource, or
`busy` if a child remains outstanding. Stdin is `none`, `inherit` (the caller's
remaining stdin descriptor), or `reader` (host-pumped bounded input). With
`inherit`, the gateway pump can read ahead; the parent must not read its stdin
after `run(inherit)`. The gateway yields
the child's bounded UTF-8 script output at exit; EOF precedes status. `wait`
consumes status once and discards unread stdout. The child shares its parent's
identity, trace and script-tree budget; each effect still needs broker approval.

The root package imports `dekopon:stdio/streams@0.1.0.reader`; `wkg.toml`
resolves it from `deps/stdio.wit`. Keep the SDK and broker-host mirrors identical
to `spawn.wit`. A nonempty child output gains a trailing newline; an ordinary
child's `exit.stderr` is empty because the shell has no separate script stderr,
and a child panic exits 70. The release workflow validates the package and checks the
immutable published bytes before accepting a duplicate version.
