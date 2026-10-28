# Approach: awk signed non-finite formatting

Steering: "# Steering for GPT executors" (7a5ceaf641b3516be8cfe7b2ff0769ad6690f598).
Base: 2e0a398620c6ceaf77b0b0cd3353d1d9fa66c0fc.
Branch: codex/awk-nonfinite-1.

## Conflicts and readings

- Original delivery says "No approach note"; the coordinator follow-up requires an approach note and coordinator-launched Astra note review before the build. Follow the later instruction; no build until the coordinator reports acceptance.
- Base format.rs:337-344,448-451 spells signed NaN using Rust formatting and zero-pads non-finite values, contrary to DECISIONS-r7 sections 2.5/4 as clarified by F1. Change only the signed conversion fallback.
- DECISIONS-r7 section 8.2 asks for Interpreter tests; this brief expressly permits formatter tests and requires explicitly signed NaNs. Use formatter coverage for explicit NaN signs and Interpreter coverage for infinity and large finite values.
- Repository guidance says current main; dispatch fixes the exact base SHA. Branch from the pinned base.
- Steering asks for a full final report; the brief asks for a parent message of at most 300 words. Follow the brief's report scope; link detailed evidence in the PR.
- PR template requires Security impact while steering discourages boilerplate security sections. Keep the required template heading with a concrete sentence limiting the changed behavior to awk signed formatting.

## Implementation

Add a guarded Conversion::Signed arm in conversion, before the existing integer arm. Select NaN, infinity and magnitudes at least 2^63 with an explicit non-finite check or magnitude comparison against 9223372036854775808.0, mirroring the exact literal already used in number(). Render with floating(out, n, Conversion::Fixed(false), 0, spec.flags). Return the existing floating arm's sign-prefix length and spec.flags.zero && n.is_finite(). Leave integer(), floating(), every other conversion, and every refusal untouched. No dependency, module, parser, documentation-contract, WIT, config or telemetry change.

## Tests

- In the existing awk tests module, exercise format::write with f64::NAN.copysign(1.0), f64::NAN.copysign(-1.0), f64::INFINITY and f64::NEG_INFINITY. For both d and i, assert lowercase spelling, signs, and space padding under 0. Cover +/space sign prefixes and left alignment within this same invariant test. The old code must fail these assertions on NaN spelling and non-finite zero padding.
- Beside neighboring formatting tests in interpreter_tests.rs, reuse stdout/run helpers for %06d/%06i of log(0) and signed conversion of +/-2^63, with zero width padding and explicit integer precision. Include the largest represented value below 2^63 with integer precision to distinguish the fallback from the unchanged integer path. Force the finite fallback's %.0f behavior to be observable using a precision wider than its digits: integer() would add precision zeros; the floating fallback uses precision 0.
- Keep the existing golden formatter rows unchanged, including %+08.3d and %-08d for 12.

## Gates and delivery

Only after coordinator-launched Astra note review accepts: git diff --check; cargo fmt --all --check; fixture fetch; focused new tests; cargo test -p dekopon-shell --locked; workspace clippy with all targets/features and warnings denied; workspace tests --all-features --locked --lib --bins --tests with documented fixture variables; rustdoc with -D warnings; cargo deny --all-features check. Record exact outcomes and any unavailable gates. Verify new regressions fail with the production fix removed, then restore it. Check CI on the exact pushed head. Push one commit with the mandated Changelog: Fixed trailer and open the one PR ready only after all gates pass. Coordinator launches fresh Astra and Claude head reviews, each building and running the tests plus an Interpreter::run printf probe. No merge, tag, release, deploy, main push or amend of a pushed commit.

## Choices I made

- Follow the coordinator's approach-note follow-up over the earlier "No approach note" sentence.
- Express 2^63 with the exact decimal f64 literal already used by number(), with an explicit non-finite check.
- Put the fallback in conversion so signed values reuse floating formatting and padding while integer and other conversions stay untouched.
- Put explicit-sign NaN coverage in the existing awk tests module and public Interpreter probes beside neighboring formatting tests; reuse existing helpers where available.
- Use integer precision above the digit count to make the large-finite fallback test fail on the old path, while retaining the existing small-integer golden rows.
