# Fable review brief: multi-LLM implementation plan

**Status: Exploration.** Review the [implementation plan](multi-llm-implementation-plan.md) before implementation. This is a design review, not authorization to implement, deploy, record paid calls, change fixture policy or broaden scope.

## Copyable assignment

> Review `docs/multi-llm-implementation-plan.md` against the current source and repository rules. The goal is a lightweight, idiomatic Rust inference API that runs the same Dekopon loop on Codex subscription and OpenRouter, preserves existing OpenAI-compatible clients, uses async reqwest plus common tracing, and fails clearly on incompatibility. Vertex Gemini/Claude and Bedrock are future extensions, not required implementations.
>
> Prefer deleting unnecessary architecture over adding a framework. Do not turn a few VCR-style interactions into a conformance campaign. We expect to fix real provider quirks from traces in later development cycles, outside any one chat.
>
> Read the plan first, then inspect only the relevant model, prompt-loop, gateway-config/session/cancellation, credential and tracing seams. Judge whether each milestone has a coherent implementable exit. Identify concrete contradictions or failure cases; do not speculate about every imaginable future provider. No code changes or live calls.
>
> Return a verdict, material findings with plan section plus source symbol/path and smallest correction, a short list of things to remove/defer, and owner decisions genuinely needed before implementation. Separate existing defects from defects introduced by the plan. “No findings” is valid. Do not rewrite the plan unless asked.

## Acceptance criteria

1. **Same loop, useful first release.** Codex and arbitrary syntactically valid OpenRouter model IDs are selectable in config. Existing OpenAI-compatible behavior survives. No hardcoded online catalog or provider-specific branches in tool orchestration. The example exercises the actual core loop with a harmless tool, not just two bare completion calls.
2. **Rust types earn their place.** Native async/RPITIT trait, concrete adapters and closed runtime enum are sufficient. Role/call IDs, provider options, cache TTLs and continuation prevent meaningful misuse. No public unused types, `async_trait`, boxed-future framework, raw provider-options bag, GAT hierarchy or cloud SDK scaffolding without a real need.
3. **The bridge is real, not async paint.** Inspect `ChatModel`, `ScriptRuntime`, `run_prompt_session`, gateway `spawn_blocking`, callback captures and `SessionCancellation`. Can the explicit runtime handle/Send/lifetime/cancellation design work without blocking Tokio workers, creating a runtime per request, losing trace context, holding guards across await or duplicating the loop? Is the retained blocking bridge a reasonable scope tradeoff? A concrete obstacle needs a fix, not an automatic shell rewrite.
4. **Config is predictable.** Existing settings remain meaningful, unknown authored fields and local invalid combinations fail at startup with all independent conflicts reported, and secrets are references. OpenRouter defaults are usable; more esoteric models are not excluded by registry policy. Local checks do not pretend to certify remote support. Codex does not inherit public OpenAI TTL fields by model name.
5. **Replay and tools retain meaning.** Codex native items and OpenRouter reasoning details survive a complete tool loop. Visible progress cannot contain reasoning/tool arguments. Partial calls are never executed, and switching route/upstream cannot silently strip required continuation. Provider-side tools and retries must not acquire effect authority.
6. **One generation transport, no gratuitous auth migration.** reqwest pooling, TLS, redirects, cancellation, deadlines and error handling have one unprivileged home. The gateway must not depend on `dekopon-http-host`. Keeping existing bounded Codex auth/refresh behind the blocking boundary is deliberate. Check that releasing auth locks before generation preserves the single rotating-token implementation shared with the broker.
7. **Fail fast without destroying diagnostics.** Typed errors preserve phase, causes and safe provider diagnostics. HTTP-200 stream errors, truncated streams and tool JSON failure do not become success. The planned removal of Codex's current 401 resend is explicit and implementable; token invalidation permits a later independent attempt without an infinite stale-token failure. No SDK/client retry or router fallback hides an error.
8. **Performance has a simple path.** Clients/pools reused; response streamed; callback cheap; no extra full prompt/asset copy; blocking asset/auth work not performed on runtime workers; one bounded owner per growing buffer. Do not demand latency benchmarks to approve the architecture, or introduce batching/schedulers/prefetch before a measured need.
9. **Comparable traces are honest.** Same identity, duration, first-visible-text, tool count, cache usage and error semantics across providers. Missing is unknown, not zero. Native counters normalize without double-counting. Requested controls are not asserted as remotely honored. Subscriber/exporter stays outside the library; existing per-message trace and complete audit survive. No secrets or attachment bytes leak; no traceparent sent to inference endpoints.
10. **Tests are small but meaningful.** A couple of two-turn cassette sequences exercise real serialization/replay, not only deserialization. One stalled-body cancellation test covers what buffered playback cannot. Existing regressions are reused, not multiplied across a provider/model Cartesian product. No auto-record/miss-to-network in CI. Respect the current ban on committed fetched provider fixtures; local recordings and synthetic committed cassettes are distinct, and secret removal occurs before persistence.
11. **Future adapters don't dictate today's API.** Adding Vertex/Bedrock should have a clear trait/codec/config/error/fixture recipe. Named-resource cache administration and non-SSE/cloud auth differences are acknowledged but deferred. No promise that all future providers must use identical transport plumbing. External reuse is plausible without moving crates or installing global configuration.

## Finding format and bar

For each finding:

- **Category:** `goal-breaking`, `regression`, or `optional`; add `contract`, `guideline`, or `taste` when invoking repository rules.
- **Evidence:** plan section plus current source symbol/path or exact rule. Name the concrete scenario that fails.
- **Correction:** smallest change that meets the user's scope. State if it removes work rather than adds it.
- **Disposition:** must fix before implementation, decision needed, or defer.

Do not require a quota of findings. A documented unknown that safely fails is not a blocker. A future cloud model's undocumented TTL is not a reason to block Codex/OpenRouter. An unsupported combination accepted and silently dropped **is** a blocker.

## Decisions worth highlighting, not silently overruling

- Is the synchronous-loop/native-async-client bridge the right first-release tradeoff, given the real callback/runtime types?
- Are the selected OpenRouter cache modes small enough for the first release, or should a concrete one be deferred without losing the user's experimentation goal?
- Is removal of the one existing Codex 401 resend the intended fail-fast behavior, with safe next-attempt credential handling?
- If actual fetched cassettes are wanted in Git, should the explicit repository fixture prohibition change? The current plan does not change it.

End with **READY**, **READY WITH NONBLOCKING NOTES**, or **REVISE**, and a short reason. This verdict is about the plan, not implementation correctness, live provider compatibility, deployment health or authority to merge.
