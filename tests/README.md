# Workspace integration tests

Executable CLI integration tests live with their packages in [`crates/dekopon/tests`](../crates/dekopon/tests) and [`crates/dekopon-run/tests`](../crates/dekopon-run/tests).

The repository-level black-box observability test lives with its runnable example at [`examples/otel-traces/smoke-test.sh`](../examples/otel-traces/smoke-test.sh). It starts one OpenObserve container, runs an instrumented provider invocation, and searches for expected payload-redacted traces.
