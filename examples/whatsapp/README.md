# WhatsApp Cloud API transport (placeholder setup)

This example contains no credential values and makes no live Meta calls. It shows the operator-owned
pieces for Dekopon's WhatsApp Cloud API text and image transport.

## Network

Terminate public HTTPS outside `dekopond`:

```text
Meta -> Cloudflare Tunnel -> Traefik -> dekopon ClusterIP Service -> dekopond 0.0.0.0:9080
```

Route only `/webhooks/whatsapp` to the configured listener. Do not expose another daemon path;
the listener has only GET subscription verification and signed POST delivery routes. `dekopond`
does not terminate TLS.

Enable the chart's cluster-local target on the same port as `bind`:

```yaml
gateway:
  service:
    enabled: true
    port: 9080
```

A Kubernetes Service cannot reach a loopback listener, which is why `dekopond.yaml` binds
`0.0.0.0:9080`. The chart creates no Ingress; keep the Traefik Host plus exact-path route in the
operator-owned deployment repository.

## Secrets

Create three independent high-entropy values and inject them through environment variables:

- `DEKOPOND_WHATSAPP_APP_SECRET`: the Meta app secret used only for webhook HMAC verification;
- `DEKOPOND_WHATSAPP_VERIFY_TOKEN`: an operator-generated subscription verification token; and
- `DEKOPOND_WHATSAPP_ACCESS_TOKEN`: a production system-user access token with the narrow
  `whatsapp_business_messaging` permission needed to answer.

The YAML names those variables; never paste values into it. The gateway holds these chat transport
credentials and model credentials only. Provider credentials and policy remain in
`dekopon-brokerd`.

## Meta configuration

1. Replace the placeholder WABA and receiving phone-number IDs in `dekopond.yaml`.
2. Pin a currently supported Graph API version after checking Meta's current documentation.
3. Configure the public HTTPS callback URL ending in `/webhooks/whatsapp` and enter the same
   verification token delivered through the environment.
4. Subscribe the app/WABA to message webhooks and assign the receiving phone number and sending
   permission to the system-user token.
5. Add an owner-controlled broker identity mapping for each expected canonical sender,
   `whatsapp.<wa_id>`, and a `via`-scoped `agent.prompt` policy grant. A signed webhook does not mint
   a principal or bypass broker policy.

PNG/JPEG photos with an optional caption and provider-produced PNG replies are supported. Video,
documents, stickers, templates, interactive messages,
reactions, status processing, business-management APIs, embedded signup, webhook multiplexing, and
TLS termination are out of scope. Meta rejects free-form replies outside its customer-service
window; Dekopon does not fall back to templates.

Webhook message-ID deduplication is bounded and process-local. A duplicate observed by one running
process is acknowledged without a second session; a restart forgets the set. A crash after HTTP 200
and before the in-memory queue drains loses the accepted message. At-most-once within one process
window, not durable exactly-once delivery.

## Image editing and generation

The YAML shows an image-capable model placeholder and an `image-editor` catalog agent. Configure
that agent and install the external GPT-image provider in the **broker**, with owner-authored
constraints, credential binding, and policy for `gpt-image.edit` (and `gpt-image.generate` if desired).
This example does not install a provider or grant authority. Do not put its API key in the gateway.

`chatAssetInputs: [gpt-image.edit]` lets the model propose
`{"prompt":"Make the sky purple","images":["chat-asset:1"]}` using a numbered photo reference.
`providerAttachments` returns the authorized provider's PNG without putting image bytes in the
shell, result transcript, or conversation history. A photo without a caption is still admitted;
the agent can ask what edit is wanted. Keep the two opt-ins absent for a text-only route.

Images are limited to **5,000,000 bytes** in either direction (smaller than the generic 8 MiB
attachment slot). Oversized output is refused before any upload/send; no transcoding occurs.
Short answers caption the first image (1,024 Unicode scalars); longer answers follow as complete
split text. Upload is not delivery, and an accepted message ID is not human receipt. Partial
replies create no durable delivered-turn record, and no effect is automatically retried.

The download policy permits only exact HTTPS `lookaside.fbsbx.com:443` at
`/whatsapp_business/attachments/`, without redirects or proxies, using validated public DNS results.
Other CDN hosts fail closed; this is deliberately not an exhaustive Meta compatibility claim.
The request shapes are pinned by loopback tests, not live Meta validation. The example pins v25.0;
a dashboard webhook version does not by itself establish Graph version compatibility. See the
[transport contract](../../docs/dekopond.md#meta-whatsapp-cloud-api) for bounds and limitations.
