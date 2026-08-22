# WhatsApp Cloud API transport (placeholder setup)

This example contains no credential values and makes no live Meta calls. It shows the operator-owned
pieces for Dekopon's first text-only WhatsApp Cloud API transport.

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

Only ordinary inbound and outbound text is implemented. Media, templates, interactive messages,
reactions, status processing, business-management APIs, embedded signup, webhook multiplexing, and
TLS termination are out of scope. Free-form replies can still be rejected by Meta outside its
customer-service window; Dekopon does not fall back to templates.

Webhook message-ID deduplication is bounded and process-local. A duplicate observed by one running
process is acknowledged without a second session, but a restart forgets the set. Conversely, a
crash after HTTP 200 and before the in-memory queue is drained can lose an accepted message. This is
at-most-once within one process window, not durable exactly-once delivery.
