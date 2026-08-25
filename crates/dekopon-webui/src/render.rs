use std::fmt::Write as _;

use dekopon_broker_host::{
    BrokerHostStats, ComponentInterfaceItem, LoadedProviderMetadata, ProviderCapability,
};
use dekopon_broker_protocol::{Permission, ReportedAgent, ReportedAgentCapability};

use crate::{Dashboard, OtelSummary, ProviderPage};

const CSS: &str = r#"
:root{color-scheme:light dark;--bg:#f7f7f8;--panel:#fff;--text:#202124;--muted:#62676f;--border:#d8dce2;--link:#0969da;--code:#f1f3f5;--accent:#6f42c1;--good:#1a7f37;--warn:#9a6700}
@media(prefers-color-scheme:dark){:root{--bg:#111318;--panel:#191c22;--text:#e6edf3;--muted:#9da7b3;--border:#343a46;--link:#58a6ff;--code:#242830;--accent:#bc8cff;--good:#3fb950;--warn:#d29922}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:15px/1.55 system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}code,pre,.mono{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}header{border-bottom:1px solid var(--border);background:var(--panel)}.top{max-width:1240px;margin:auto;padding:20px 28px;display:flex;align-items:baseline;gap:16px}.brand{font-size:22px;font-weight:700;color:var(--text)}.version{color:var(--muted);font-size:13px}.layout{max-width:1240px;margin:auto;display:grid;grid-template-columns:220px minmax(0,1fr);gap:30px;padding:28px}.nav{position:sticky;top:20px;align-self:start}.nav h3{font-size:12px;text-transform:uppercase;letter-spacing:.08em;color:var(--muted);margin:18px 0 7px}.nav a{display:block;padding:3px 0}.content{min-width:0}h1{font-size:30px;margin:0 0 8px}h2{font-size:22px;margin:36px 0 12px;border-bottom:1px solid var(--border);padding-bottom:7px}h3{font-size:17px;margin:24px 0 8px}p{margin:7px 0 13px}.muted{color:var(--muted)}.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(170px,1fr));gap:12px;margin:20px 0}.card,.doc{background:var(--panel);border:1px solid var(--border);border-radius:7px;padding:16px}.metric{font-size:27px;font-weight:700}.label{color:var(--muted);font-size:13px}.submetric{font-size:12px;color:var(--muted);margin-top:4px}table{border-collapse:collapse;width:100%;background:var(--panel);border:1px solid var(--border)}th,td{text-align:left;vertical-align:top;padding:9px 11px;border-bottom:1px solid var(--border)}th{font-size:12px;text-transform:uppercase;letter-spacing:.04em;color:var(--muted)}tr:last-child td{border-bottom:0}.table-wrap{overflow-x:auto}.badge{display:inline-block;border:1px solid var(--border);border-radius:999px;padding:1px 7px;margin:1px 3px 1px 0;font-size:12px;white-space:nowrap}.good{color:var(--good)}.warn{color:var(--warn)}pre{background:var(--code);border:1px solid var(--border);border-radius:6px;padding:13px;overflow:auto;font-size:13px;line-height:1.45}.item{margin:22px 0;padding-left:14px;border-left:4px solid var(--accent)}.signature{font-weight:600;font-size:16px}.kv{display:grid;grid-template-columns:minmax(130px,190px) minmax(0,1fr);gap:6px 16px}.kv dt{color:var(--muted)}.kv dd{margin:0;overflow-wrap:anywhere}.tree,.tree ul{list-style:none;margin:5px 0;padding-left:18px}.tree>li{padding-left:0}.tree code{overflow-wrap:anywhere}.empty{padding:18px;border:1px dashed var(--border);border-radius:6px;color:var(--muted)}footer{max-width:1240px;margin:0 auto;padding:15px 28px 35px;color:var(--muted);font-size:12px}@media(max-width:760px){.layout{grid-template-columns:1fr;padding:20px}.nav{position:static;border-bottom:1px solid var(--border);padding-bottom:14px}.top{padding:16px 20px}.kv{grid-template-columns:1fr}.kv dd{margin-bottom:7px}}
"#;

pub(crate) fn dashboard(dashboard: &Dashboard) -> String {
    let (inventory, inventory_reports) = dashboard.service_status.agents();
    let tokens = dashboard.service_status.tokens();
    let host = dashboard.host_metrics.snapshot();
    // Sections are written straight into the finished page rather than into a second buffer the
    // page then copies: this body is the largest allocation the crate makes per request.
    let mut content = begin_page(&dashboard.version, "Dekopon service", dashboard_nav());
    write!(
        content,
        "<h1>Dekopon service</h1><p class=muted>Live, read-only broker view. Counters reset when this broker process restarts.</p>"
    )
    .expect("writing to a String cannot fail");
    write!(
        content,
        "<div class=cards>{}{}{}{}</div>",
        metric_card(
            "Tokens in",
            tokens.input_tokens,
            missing(tokens.reports, tokens.input_unreported_calls)
        ),
        metric_card(
            "Tokens out",
            tokens.output_tokens,
            missing(tokens.reports, tokens.output_unreported_calls)
        ),
        metric_card(
            "Loaded providers",
            host.providers_loaded,
            format!("{} capabilities", provider_capability_count(dashboard))
        ),
        metric_card(
            "Wasm invocations",
            host.invocations_started,
            format!(
                "{} succeeded · {} failed",
                number(host.invocations_succeeded),
                number(host.invocations_failed)
            )
        )
    )
    .expect("writing to a String cannot fail");
    content.push_str("<p class=muted>Token totals are best-effort provider reports received from <code>dekopond</code>; they do not include standalone <code>dekopon-run</code> sessions and are not billing reconciliation.</p>");

    content.push_str("<section id=agents><h2>Agents</h2>");
    if inventory.agents.is_empty() {
        content.push_str("<div class=empty>No agent inventory has been reported by <code>dekopond</code> yet.</div>");
    } else {
        write!(
            content,
            "<p class=muted>Informational catalog surface reported by the gateway ({} report{} received). These declarations permit proposals; broker policy still authorizes every invocation.</p>",
            number(inventory_reports),
            if inventory_reports == 1 { "" } else { "s" }
        )
        .expect("writing to a String cannot fail");
        if inventory.truncated {
            content.push_str("<p class=warn>The gateway inventory reached a defensive reporting bound; this list is incomplete.</p>");
        }
        content.push_str("<div class=table-wrap><table><thead><tr><th>Agent</th><th>Providers</th><th>Capabilities and provider permissions</th></tr></thead><tbody>");
        for agent in &inventory.agents {
            render_agent_row(&mut content, dashboard, agent);
        }
        content.push_str("</tbody></table></div>");
    }
    content.push_str("</section>");

    content.push_str("<section id=providers><h2>Providers</h2><p class=muted>Validated manifests returned by components compiled into this broker.</p>");
    content.push_str("<div class=table-wrap><table><thead><tr><th>Provider</th><th>Description</th><th>Capabilities</th><th>Artifact</th></tr></thead><tbody>");
    for provider in dashboard.providers.iter().map(ProviderPage::metadata) {
        write!(
            content,
            "<tr><td><a class=mono href=\"/ui/providers/{}\">{}</a></td><td>{}</td><td>{}</td><td><span class=mono>{}</span><br><span class=muted>{}</span></td></tr>",
            escape(provider.manifest.id.as_str()),
            escape(provider.manifest.id.as_str()),
            escape(&provider.manifest.description),
            number(u64::try_from(provider.manifest.capabilities.len()).unwrap_or(u64::MAX)),
            escape(file_name(provider)),
            bytes(provider.artifact_bytes),
        )
        .expect("writing to a String cannot fail");
    }
    content.push_str("</tbody></table></div></section>");

    render_wasmtime(&mut content, &host);
    render_otel(&mut content, dashboard.otel.as_ref());
    finish_page(&mut content);
    content
}

/// Renders one provider's complete page.
///
/// Every input is fixed at broker startup, so `Dashboard::new` calls this once per provider and
/// serves the bytes afterwards; nothing on this page is live.
pub(crate) fn provider_page(version: &str, provider: &LoadedProviderMetadata) -> String {
    let mut nav = String::from(
        "<h3>Provider</h3><a href=#artifact>Artifact</a><a href=#interface>Component interface</a><a href=#capabilities>Capabilities</a><a href=#manifest>Complete manifest</a><h3>Capabilities</h3>",
    );
    for capability in &provider.manifest.capabilities {
        write!(
            nav,
            "<a href=\"#cap-{}\"><code>{}</code></a>",
            escape(capability.id.as_str()),
            escape(capability.id.as_str())
        )
        .expect("writing to a String cannot fail");
    }

    let mut content = begin_page(
        version,
        &format!("{} · Dekopon", provider.manifest.id),
        &nav,
    );
    write!(
        content,
        "<p><a href=/ui>← Service overview</a></p><h1><span class=mono>{}</span></h1><p>{}</p>",
        escape(provider.manifest.id.as_str()),
        escape(&provider.manifest.description)
    )
    .expect("writing to a String cannot fail");

    content.push_str("<section id=artifact><h2>Artifact</h2><div class=doc><dl class=kv>");
    definition(&mut content, "Runtime format", "WebAssembly Component");
    definition(
        &mut content,
        "Source path",
        &format!(
            "<code>{}</code>",
            escape(&provider.source.display().to_string())
        ),
    );
    definition(
        &mut content,
        "Source bytes",
        &bytes(provider.artifact_bytes),
    );
    definition(
        &mut content,
        "SHA-256",
        &format!("<code>{}</code>", escape(&provider.artifact_sha256)),
    );
    definition(
        &mut content,
        "Manifest API",
        // The serde spelling, not the Rust variant name: the complete manifest lower on this same
        // page prints the wire value, and two identifiers for one field is an operator trap.
        &format!("<code>{}</code>", escape(&api_version(provider))),
    );
    definition(
        &mut content,
        "Command words",
        &badges(provider.manifest.command_words.iter().map(String::as_str)),
    );
    content.push_str("</dl></div><p class=muted>The operational view receives the local Wasm component path and exact compiled-buffer digest. A managed provider lock separately retains its OCI source and manifest digest, but that provenance context is not yet joined into this read-only view.</p></section>");

    content.push_str("<section id=interface><h2>Component interface</h2>");
    if provider.interface_truncated {
        content.push_str("<p class=warn>Interface display reached its defensive item or nesting bound; the compiled component remains loaded.</p>");
    }
    content.push_str("<h3>Imports</h3>");
    interface(&mut content, &provider.imports);
    content.push_str("<h3>Exports</h3>");
    interface(&mut content, &provider.exports);
    content.push_str("</section>");

    content.push_str("<section id=capabilities><h2>Capabilities</h2>");
    for capability in &provider.manifest.capabilities {
        render_capability(&mut content, capability);
    }
    content.push_str("</section>");

    content.push_str("<section id=manifest><h2>Complete manifest</h2><p class=muted>The exact validated manifest returned by <code>describe</code> at startup.</p><pre><code>");
    let manifest = serde_json::to_string_pretty(&provider.manifest)
        .unwrap_or_else(|_| "<manifest serialization failed>".to_owned());
    content.push_str(&escape(&manifest));
    content.push_str("</code></pre></section>");

    finish_page(&mut content);
    content
}

pub(crate) fn not_found(dashboard: &Dashboard, message: &str) -> String {
    let mut content = begin_page(&dashboard.version, message, dashboard_nav());
    write!(
        content,
        "<h1>{}</h1><p>The requested informational page does not exist.</p><p><a href=/ui>Return to the service overview</a></p>",
        escape(message)
    )
    .expect("writing to a String cannot fail");
    finish_page(&mut content);
    content
}

fn api_version(provider: &LoadedProviderMetadata) -> String {
    serde_json::to_value(provider.manifest.api_version)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "<unknown manifest API version>".to_owned())
}

fn render_agent_row(output: &mut String, dashboard: &Dashboard, agent: &ReportedAgent) {
    let status = if agent.enabled {
        "<span class=\"badge good\">enabled</span>"
    } else {
        "<span class=\"badge warn\">disabled</span>"
    };
    write!(
        output,
        "<tr><td><strong class=mono>{}</strong> {}<br><span class=muted>{}</span>{}</td><td>{}</td><td>",
        escape(agent.id.as_str()),
        status,
        escape(&agent.description),
        agent.model_class.as_ref().map_or_else(String::new, |class| format!("<br><span class=muted>model class:</span> <code>{}</code>", escape(class))),
        agent_provider_badges(dashboard, agent),
    )
    .expect("writing to a String cannot fail");
    if agent.capabilities.is_empty() {
        output.push_str("<span class=muted>none declared</span>");
    } else {
        output.push_str("<ul class=tree>");
        for capability in &agent.capabilities {
            render_agent_capability(output, dashboard, capability);
        }
        output.push_str("</ul>");
    }
    output.push_str("</td></tr>");
}

fn render_agent_capability(
    output: &mut String,
    dashboard: &Dashboard,
    capability: &ReportedAgentCapability,
) {
    write!(
        output,
        "<li><code>{}</code> <span class=muted>via {}</span>",
        escape(capability.id.as_str()),
        escape(capability.provider.as_str())
    )
    .expect("writing to a String cannot fail");
    if !dashboard
        .providers
        .iter()
        .map(ProviderPage::metadata)
        .any(|provider| {
            provider.manifest.id == capability.provider
                && provider
                    .manifest
                    .capabilities
                    .iter()
                    .any(|loaded| loaded.id == capability.id)
        })
    {
        output.push_str(" <span class=\"badge warn\">not loaded</span>");
    }
    if !capability.permissions.is_empty() {
        output.push_str("<ul>");
        for permission in &capability.permissions {
            write!(output, "<li>{}</li>", permission_text(permission))
                .expect("writing to a String cannot fail");
        }
        output.push_str("</ul>");
    }
    output.push_str("</li>");
}

fn agent_provider_badges(dashboard: &Dashboard, agent: &ReportedAgent) -> String {
    if agent.providers.is_empty() {
        return "<span class=muted>none</span>".to_owned();
    }
    let mut output = String::new();
    for provider in &agent.providers {
        let loaded = dashboard
            .providers
            .iter()
            .map(ProviderPage::metadata)
            .any(|loaded| loaded.manifest.id == *provider);
        write!(
            output,
            "<span class=\"badge{}\">{}{}</span>",
            if loaded { " good" } else { " warn" },
            escape(provider.as_str()),
            if loaded { "" } else { " · not loaded" }
        )
        .expect("writing to a String cannot fail");
    }
    output
}

fn permission_text(permission: &Permission) -> String {
    match &permission.resource {
        Some(resource) => format!(
            "<code>{}</code> on <code>{}</code>",
            escape(&permission.operation),
            escape(resource)
        ),
        None => format!("<code>{}</code>", escape(&permission.operation)),
    }
}

fn render_capability(output: &mut String, capability: &ProviderCapability) {
    write!(
        output,
        "<article class=item id=\"cap-{}\"><div class=signature>pub capability <code>{}</code></div><p>{}</p><p><span class=badge>{}</span><span class=badge>risk {:?}</span><span class=badge>{}</span></p><h3>Input schema</h3><pre><code>{}</code></pre></article>",
        escape(capability.id.as_str()),
        escape(capability.id.as_str()),
        escape(&capability.description),
        escape(&capability.effect.to_string()),
        capability.risk,
        escape(&capability.idempotency.to_string()),
        escape(&serde_json::to_string_pretty(&capability.input_schema).unwrap_or_else(|_| "<schema serialization failed>".to_owned())),
    )
    .expect("writing to a String cannot fail");
}

fn render_wasmtime(output: &mut String, stats: &BrokerHostStats) {
    output.push_str("<section id=wasmtime><h2>Wasmtime</h2><p class=muted>Host-observed process counters and startup-fixed ceilings. Wasmtime does not expose allocator-wide resident memory or JIT cache internals through this embedding API; memory/table values below are resource-limiter requests.</p>");
    output.push_str("<div class=cards>");
    for (label, value, detail) in [
        (
            "Stores created",
            stats.stores_created,
            format!(
                "{} active · {} peak",
                number(stats.active_stores),
                number(stats.peak_active_stores)
            ),
        ),
        (
            "Instantiations",
            stats.component_instantiations,
            "fresh instance per operation".to_owned(),
        ),
        (
            "Fuel consumed",
            stats.fuel_consumed,
            format!(
                "{} supplied across {} observations",
                number(stats.fuel_supplied),
                number(stats.fuel_observations)
            ),
        ),
        (
            "HTTP requests",
            stats.http_requests,
            format!(
                "{} sent · {} received",
                bytes(stats.http_request_bytes),
                bytes(stats.http_response_bytes)
            ),
        ),
        (
            "Storage operations",
            stats.storage_operations,
            format!(
                "{} invocations · {} syncs · {} quota denials",
                number(stats.storage_invocations),
                number(stats.storage_syncs),
                number(stats.storage_quota_denials)
            ),
        ),
    ] {
        output.push_str(&metric_card(label, value, detail));
    }
    output.push_str("</div>");
    output.push_str("<h3>Runtime counters</h3><div class=table-wrap><table><tbody>");
    for (label, value) in [
        ("Components compiled", stats.component_compilations),
        ("Compilation time (µs)", stats.compilation_micros),
        ("Artifact bytes", stats.artifact_bytes),
        ("Manifest descriptions", stats.provider_descriptions),
        ("Command resolutions", stats.command_resolutions),
        ("Invocations started", stats.invocations_started),
        ("Invocations succeeded", stats.invocations_succeeded),
        ("Invocations failed", stats.invocations_failed),
        ("Invocations timed out", stats.invocations_timed_out),
        (
            "Provider input bytes (non-storage)",
            stats.provider_input_bytes,
        ),
        (
            "Provider output bytes (non-storage)",
            stats.provider_output_bytes,
        ),
        (
            "Storage read byte bucket max",
            stats.storage_read_bucket_max,
        ),
        (
            "Storage write byte bucket max",
            stats.storage_write_bucket_max,
        ),
        ("Memory growth requests", stats.memory_growth_requests),
        ("Memory growth denied", stats.memory_growth_denied),
        ("Memory growth failed", stats.memory_growth_failed),
        (
            "Peak bytes requested by one memory",
            stats.peak_memory_bytes_requested,
        ),
        ("Table growth requests", stats.table_growth_requests),
        ("Table growth denied", stats.table_growth_denied),
        ("Table growth failed", stats.table_growth_failed),
        (
            "Peak elements requested by one table",
            stats.peak_table_elements_requested,
        ),
    ] {
        write!(
            output,
            "<tr><th>{}</th><td class=mono>{}</td></tr>",
            escape(label),
            number(value)
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("</tbody></table></div><h3>Engine and store configuration</h3><div class=table-wrap><table><tbody>");
    for (label, value) in [
        ("Component model", "enabled".to_owned()),
        ("Async support", "enabled".to_owned()),
        ("Fuel consumption", "enabled".to_owned()),
        ("Fuel per store", number(stats.limits.fuel)),
        (
            "Fuel yield interval",
            number(stats.limits.fuel_yield_interval()),
        ),
        (
            "Max memory per linear memory",
            bytes(u64::try_from(stats.limits.max_memory_bytes).unwrap_or(u64::MAX)),
        ),
        (
            "Max table elements",
            number(u64::try_from(stats.limits.max_table_elements).unwrap_or(u64::MAX)),
        ),
        (
            "Max instances per store",
            number(u64::try_from(stats.limits.max_instances).unwrap_or(u64::MAX)),
        ),
        (
            "Max tables per store",
            number(u64::try_from(stats.limits.max_tables).unwrap_or(u64::MAX)),
        ),
        (
            "Max memories per store",
            number(u64::try_from(stats.limits.max_memories).unwrap_or(u64::MAX)),
        ),
        (
            "Max provider input",
            bytes(u64::try_from(stats.limits.max_input_bytes).unwrap_or(u64::MAX)),
        ),
        (
            "Max provider output",
            bytes(u64::try_from(stats.limits.max_output_bytes).unwrap_or(u64::MAX)),
        ),
        (
            "Max HTTP requests",
            number(u64::from(stats.limits.max_http_requests)),
        ),
        (
            "Max HTTP request bytes",
            bytes(stats.limits.max_http_request_bytes),
        ),
        (
            "Max HTTP response bytes",
            bytes(stats.limits.max_http_response_bytes),
        ),
        (
            "Max HTTP headers",
            number(u64::try_from(stats.limits.max_http_headers).unwrap_or(u64::MAX)),
        ),
        (
            "Max HTTP header bytes",
            bytes(u64::try_from(stats.limits.max_http_header_bytes).unwrap_or(u64::MAX)),
        ),
        (
            "Max operation timeout",
            format!("{} ms", stats.limits.max_timeout.as_millis()),
        ),
    ] {
        write!(
            output,
            "<tr><th>{}</th><td class=mono>{}</td></tr>",
            escape(label),
            escape(&value)
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("</tbody></table></div></section>");
}

fn render_otel(output: &mut String, otel: Option<&OtelSummary>) {
    output.push_str("<section id=otel><h2>OpenTelemetry</h2>");
    let Some(otel) = otel else {
        output.push_str("<div class=empty>OTLP export is disabled for <code>dekopon-brokerd</code>.</div></section>");
        return;
    };
    output.push_str("<p class=muted>Credential values and resource-attribute values are deliberately withheld. Query <code>accounting.model.turn</code> for durable token accounting and follow trace IDs for model, broker, provider, and HTTP timing.</p><div class=doc><dl class=kv>");
    definition(
        output,
        "Endpoint",
        &format!("<code>{}</code>", escape(&otel.endpoint)),
    );
    definition(
        output,
        "Transport",
        &format!("<code>{}</code>", escape(&otel.transport)),
    );
    definition(
        output,
        "Service name",
        &format!("<code>{}</code>", escape(&otel.service_name)),
    );
    definition(
        output,
        "Export timeout",
        &format!("{} ms", number(otel.export_timeout_ms)),
    );
    definition(
        output,
        "Payload telemetry",
        if otel.telemetry_payloads {
            "enabled"
        } else {
            "disabled"
        },
    );
    definition(
        output,
        "OTLP headers",
        if otel.headers_configured {
            "configured; values hidden"
        } else {
            "not configured"
        },
    );
    definition(
        output,
        "Resource attributes",
        if otel.resource_attributes_configured {
            "configured; values hidden"
        } else {
            "not configured"
        },
    );
    output.push_str("</dl></div></section>");
}

fn interface(output: &mut String, items: &[ComponentInterfaceItem]) {
    if items.is_empty() {
        output.push_str("<div class=empty>None</div>");
        return;
    }
    output.push_str("<ul class=tree>");
    for item in items {
        interface_item(output, item);
    }
    output.push_str("</ul>");
}

fn interface_item(output: &mut String, item: &ComponentInterfaceItem) {
    write!(
        output,
        "<li><span class=badge>{}</span> <code>{}</code>",
        escape(item.kind),
        escape(&item.name)
    )
    .expect("writing to a String cannot fail");
    if let Some(signature) = &item.signature {
        write!(output, " <span class=muted>{}</span>", escape(signature))
            .expect("writing to a String cannot fail");
    }
    if !item.members.is_empty() {
        output.push_str("<ul>");
        for member in &item.members {
            interface_item(output, member);
        }
        output.push_str("</ul>");
    }
    output.push_str("</li>");
}

fn definition(output: &mut String, term: &str, value_html: &str) {
    write!(output, "<dt>{}</dt><dd>{}</dd>", escape(term), value_html)
        .expect("writing to a String cannot fail");
}

fn metric_card(label: &str, value: u64, detail: String) -> String {
    format!(
        "<div class=card><div class=metric>{}</div><div class=label>{}</div><div class=submetric>{}</div></div>",
        number(value),
        escape(label),
        escape(&detail)
    )
}

fn missing(reports: u64, calls: u64) -> String {
    if reports == 0 {
        "no model-usage reports received".to_owned()
    } else if calls == 0 {
        "all received reports included this count".to_owned()
    } else {
        format!(
            "{} call{} did not report it",
            number(calls),
            if calls == 1 { "" } else { "s" }
        )
    }
}

fn provider_capability_count(dashboard: &Dashboard) -> u64 {
    dashboard
        .providers
        .iter()
        .map(ProviderPage::metadata)
        .map(|provider| u64::try_from(provider.manifest.capabilities.len()).unwrap_or(u64::MAX))
        .fold(0, u64::saturating_add)
}

fn file_name(provider: &LoadedProviderMetadata) -> &str {
    provider
        .source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<non-UTF-8 path>")
}

fn badges<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    let mut output = String::new();
    for value in values {
        write!(output, "<span class=badge>{}</span>", escape(value))
            .expect("writing to a String cannot fail");
    }
    if output.is_empty() {
        output.push_str("<span class=muted>none</span>");
    }
    output
}

fn dashboard_nav() -> &'static str {
    "<h3>Service</h3><a href=#agents>Agents</a><a href=#providers>Providers</a><a href=#wasmtime>Wasmtime</a><a href=#otel>OpenTelemetry</a>"
}

fn begin_page(version: &str, title: &str, nav: &str) -> String {
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{}</title><style>{CSS}</style></head><body><header><div class=top><a class=brand href=/ui>Dekopon</a><span class=version>brokerd {}</span></div></header><div class=layout><nav class=nav aria-label=Sections>{}</nav><main class=content>",
        escape(title),
        escape(version),
        nav
    )
}

fn finish_page(output: &mut String) {
    output.push_str(
        "</main></div><footer>Unauthenticated informational view · no mutation or authorization endpoints · counters are process-local</footer></body></html>",
    );
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other if other.is_control() => {
                write!(&mut escaped, "&#x{:X};", u32::from(other))
                    .expect("writing to a String cannot fail");
            }
            other => escaped.push(other),
        }
    }
    escaped
}

fn number(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1024.0 && unit + 1 < UNITS.len() {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", number(value), UNITS[unit])
    } else {
        format!("{scaled:.1} {}", UNITS[unit])
    }
}
