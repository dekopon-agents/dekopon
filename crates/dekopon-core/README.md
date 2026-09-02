# dekopon-core

Validated identifiers and dependency-light domain types shared by Dekopon crates. Among the
identifiers, `SkillId` follows the open Agent Skills name grammar rather than Dekopon's resource
grammar — lowercase ASCII letters, digits, and single hyphens, at most 64 bytes — so a skill
authored for another client loads here unchanged and one authored here loads there.

It also holds the small helpers that separate processes must not disagree about: the `accept()`
retry classification with its backoff bounds, `error_chain`, which renders a failure and its sources
as one line, `read_trusted_file`, the one definition of what makes a local file trusted input —
opened without following a symlink, regular, single-link, owned by this process, within a byte
ceiling, and at one of two named permission tiers — `PROVIDER_COMPONENT_EXTENSION`, the file
extension both the broker and the direct runner select components by, and `RESERVED_COMMAND_WORDS`
with `command_word_conflicts`, the shell-owned word list and the check that reports every reserved,
capability-shaped, duplicated, or repeated provider command word at once, so the broker can refuse
a conflicting manifest at startup without linking the interpreter.

This crate contains no transport, CLI, async runtime, policy-engine, or provider-host dependencies.
The `errno` table and the trusted-file predicate bring `libc` on Unix targets only, so a wasm guest
build pulls none of it; the predicate itself is Unix-only, and its callers wrap the blocking read in
one `spawn_blocking` rather than importing an async runtime here.
