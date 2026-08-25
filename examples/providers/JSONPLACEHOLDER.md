# JSONPlaceholder provider

The JSONPlaceholder provider is maintained and released independently from Dekopon core:

- Source: https://github.com/dekopon-agents/dekopon-provider-jsonplaceholder
- Exact bundled release: https://github.com/dekopon-agents/dekopon-provider-jsonplaceholder/releases/tag/v0.1.0
- Component: `jsonplaceholder-provider.wasm`
- Core checksum authority: `ci/fetch-external-provider-components.sh`

It exposes a bounded post read and a separately named non-idempotent synthetic create operation.
The component imports only `dekopon:http/client@1.0.0`; direct mode therefore rejects it, while the
broker may link it only under explicit destination, method, request-count, byte, and timeout
constraints. Tests use injected or loopback responses and do not contact the public service.

Core release and image automation download the exact v0.1.0 asset, verify its published sidecar
against the checksum pinned above, and optionally verify its GitHub attestation. No provider source
or generated Wasm is tracked in the core repository.
