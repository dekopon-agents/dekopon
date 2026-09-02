# Workspace integration tests

Integration tests live beside the crate that owns the behavior under `crates/*/tests`; shared scaffolding is `crates/dekopon-test-support`. The complete map from area to implementation and test suite is [Repository map](../docs/development.md#repository-map).

Before running the workspace tests, fetch the gitignored standalone provider fixtures; the full command set is in [Validation](../docs/development.md#validation).

```console
ci/fetch-external-provider-components.sh examples/providers
```

The repository-level black-box observability test lives with its runnable example at [`examples/otel-traces/smoke-test.sh`](../examples/otel-traces/smoke-test.sh). It builds the runner, starts one OpenObserve container, runs a direct provider invocation, and searches both streams: the trace stream for the required spans and no sentinel provider input, and the log stream for a lifecycle record carrying the same `trace_id`. It removes the container and its volume afterward ([details](../docs/development.md#openobserve-otlp-end-to-end-test)).
