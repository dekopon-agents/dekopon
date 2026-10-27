# Signed-download testkit approach

Base: 999c6c05a785c7beca52903883aece2b87f8f0a6.
Pinned steering first line: `# Steering for GPT executors`.

## Conflicts and readings

- The initial brief requires two requests and one storage call; the base performs API GET, a bytes=0-0 probe, then window/context ranges. Follow the coordinator ruling: configurable cap, success tests at least three requests; assert the ordered probe and window requests separately.
- The initial brief forbids HTTP-host/broker-host changes; the ruling authorizes test-only transport and synthetic credential hooks there. Gate hooks behind a non-default native-test feature; retain production behavior when the feature is absent.
- Native signed download currently returns Denied without consulting Port. Follow the ruling: only the native-test feature adds scoped fake dispatch; normal native builds still return Denied.
- Conformance's closed list omits signed-download. Admit that exact version as an optional HTTP import, retaining refusal of unknown imports and required ordinary imports.
- Steering discourages new public APIs for internal limits; the brief explicitly requests one script entry point. Add only the scenario builder and the minimal feature-gated cross-crate hooks it needs.
- The 250-line target predates the expanded scope. Measure the implementation/test delta and record any excess with numbers in the final PR.

## Entry point and script

Proposed builder on both Run<P> and Native<P>:

```rust
pub fn signed_download(self, api: HttpScript, storage: HttpScript, max_requests: u32) -> Self
```

Reuse HttpScript rather than add another public script type. The API script contains the exact 302 Location, including the storage path/query. Storage has an admitted hostname on HTTPS port 443. The ordinary .http helper retains max_requests=1 and signed_download=None. This scenario sets GithubActionsLogsV1 and the caller's request cap. Invalid fixture shapes return typed fixture errors.

## Transport and credentials

Add a non-default native-test feature propagated from testkit into broker-host, HTTP-host and SDK. Use a closed, invocation-owned in-process scripted transport, keyed by exact HTTPS authority. It has no socket, DNS lookup, task, unbounded queue or network fallback. Unscripted destinations refuse. Keep ordinary production transport code and the test transport behind an explicit closed dispatch.

Reuse the HTTP host's signed_download operation, location/storage_host validation, API-route checks, credential separation, request accounting, range parsing, response bounds and terminal storage-redirect refusal. Replace only network resolution/execution for registered test authorities; the fake does not implement another redirect policy. Serve a byte slice as proper 206 responses for the real probe and bounded Range requests; a scripted redirect response remains a redirect for refusal tests. Preserve the response shape consumed by the existing host readers rather than duplicate their range/security checks.

Extend TestImports with a feature-gated synthetic destination-bound credential used by invoke_with_test_imports. Bind it only to the API authority; never to storage. Testkit owns a fixed synthetic fixture value. No credential discovery or real credential is used.

Under SDK native-test, extend the existing scoped Port with a default-denied signed-download method and route native download through it. Testkit's implementation calls the same host operation/transport, translating its bounded asset result for the SDK native response. Avoid a second standalone native redirect implementation. Keep all mutable invocation state scoped to the call.

## Assertions and validation

Add ordered component request evidence beside existing http_request, preserving its ordinary behavior. Native::requests() records the API request, probe and window in order. Capture guest headers before credential injection separately from transport headers: assert no guest authorization on either destination, synthetic credential only on API transport, and none on storage.

Build a real signed-download test component with the pinned fixture tooling; load its path with expect, without fallback or skip. Assert returned bytes, exact URI including signed query on every storage request, exact probe/window call counts and request order. A second storage redirect must fail terminally without any request to its Location. Test cap exhaustion and unchanged ordinary scripts, and conformance admission of the exact import. Document each test's falsifying change when reporting coverage.

Run diff check, fmt, locked clippy/tests, rustdoc with -D warnings, cargo deny, documentation gates, and the affected fixture build/import checks. Verify feature-disabled native denial and production compilation. Report every unavailable gate. No WIT, wire/config contract, version, changelog file, release, tag or live-system change.

## Choices I made

- Use one signed_download builder on both existing harnesses, taking existing API/storage HttpScript values and an explicit cap.
- Serve admitted HTTPS:443 authorities entirely in-process through feature-gated transport hooks.
- Reuse the real host's signed-download algorithm for component and native runs.
- Preserve default HTTP caps; use at least three requests for signed-download success tests.
- Keep a synthetic API-only credential in test imports and record guest versus transport evidence separately.
- Admit the exact signed-download import as an optional HTTP-family import.
- Add ordered component evidence while preserving the existing single-request field.
- Defer implementation until the coordinator's approach-note review; delete this note in the final PR commit.
