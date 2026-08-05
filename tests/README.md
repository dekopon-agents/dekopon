# Workspace integration tests

Executable CLI integration tests live with their packages in [`crates/dekopon/tests`](../crates/dekopon/tests) and [`crates/dekopon-run/tests`](../crates/dekopon-run/tests).

[`otel-kind/e2e.sh`](otel-kind/e2e.sh) is the repository-level black-box observability test. It deploys the [Quickwit kind stack](../deploy/quickwit-kind/README.md), runs an instrumented prompt/script session, and searches both OTEL indexes for correlated, payload-redacted logs and traces.
