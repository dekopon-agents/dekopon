# dekopon-model-token-governor

`dekopon-model-token-governor` holds Dekopon's optional per-agent token budgets. It has no I/O
and no clock of its own: every method takes the time it acts at as `UnixMillis`, so tests and
the gateway's boot restore drive time with integers.

- `ModelUsage` and its normalization rules: `input_tokens` includes cache reads and cache
  writes, and `output_tokens` includes reasoning. A client normalizes before usage leaves it.
- `Meter`: `fixed` (windows at UTC epoch multiples), `rolling` (any trailing period, in buckets
  of period/60), `session` (opens on the first charge) and `credit` (a refilling bucket). A
  meter may overdraw; the debt is real spend and delays the next allow.
- `Budget`: every meter must allow. `reserve` holds an estimate so concurrent sessions cannot
  both pass and both overspend, and `settle` swaps it for the real charge. `restore` rebuilds
  fresh meters from history and replays the charges seen live since boot on top.
- `Refusal`: plain data whose `Display` is the chat sentence, for example "I'm at 99% of my
  token budget (5-hour rolling window): 10 tokens left, this message needs about 100. Try again
  in 15 minutes."
- `Metering`: the one service the agent's model calls and the guest model proxy share.
  `admit` is synchronous, takes the lock only for arithmetic and returns an `Admission` that
  holds no guard, so it can live across an `.await`. `Admission::settle` charges what was
  observed, with the estimate filling the gaps, and emits exactly one `meter` log record; an
  `Admission` dropped unsettled settles as cancelled.

A call costs `input + output` in raw tokens. The estimate is `bytes / 4` plus 1,000 tokens per
image, raised to the session's last settled input plus the new bytes, and an output reserve.

```rust
use std::{sync::Arc, time::Duration};
use dekopon_model_token_governor::{
    Budget, Call, Estimate, MeterSpec, Metering, Outcome, Sizes, Tokens, UnixMillis, Via,
};

let agent = "gylmar".parse().unwrap();
let budget = Budget::new(
    agent,
    None,
    &[MeterSpec::Rolling { limit: Tokens(200_000), period: Duration::from_secs(5 * 3_600) }],
    UnixMillis::now(),
);
let metering = Arc::new(Metering::new(vec![budget], Metering::system_clock()));
let call = Call { agent: "gylmar".parse().unwrap(), model: "astra".into(), backend: "codex", via: Via::Agent };
let admission = metering
    .admit(call, Estimate::from_sizes(Sizes { bytes: 4_000, images: 0 }, None, Tokens(1_024)))
    .expect("an empty budget admits");
admission.observe_text(120);
admission.settle(Outcome::Succeeded);
```
