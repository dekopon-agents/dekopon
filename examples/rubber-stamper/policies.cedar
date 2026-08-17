// policies.cedar — who may do what, and through which gateway. Nothing here says how narrowly
// the broker then executes it: timeouts, hosts, methods, call budgets, and the credential binding
// live in broker.yaml's constraint sets. Editing this file can broaden who may act; it can never
// widen a timeout, reach a new host, or bind a credential that was not already bound.
//
// chmod 0600, owner-owned, single-link. Validated in Cedar's strict mode at startup against a
// schema generated from broker.yaml's declared world, so a typo in a principal, provider, or
// capability name refuses startup instead of becoming a rule that silently never matches.
//
// The @id annotations are what audit records carry as `policy_ids`. Without them Cedar names
// policies positionally and inserting one renumbers the rest, which is exactly what an audit
// trail must not do.

// The session gate: may this person drive this agent at all, and through which gateway?
//
// This is checked before `capabilitiesFor` answers and before any capability is authorized, so a
// sender the owner never permitted to use this agent is refused before a model call is paid for.
@id("boss-may-prompt-rubber-stamper")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"xaviers-rubber-stamper")
when { context has via && context.via == "dekopond-gateway" };

// What that session may then reach. The two statements are independent: driving the agent implies
// none of these capabilities, and holding these capabilities implies no session.
//
// `context.via` keeps attested and direct authority disjoint. Because both conditions require the
// gateway, a process connecting to the broker socket directly under the same UID — including this
// gateway's own peer identity acting for itself — matches neither rule. Adding a gateway cannot
// widen a grant that already existed.
//
// `context.agent` pins the surface to this one agent. A second agent added to the catalog later
// starts with nothing, even under the same principal and the same gateway.
@id("rubber-stamper-gh-surface")
permit(principal == Dekopon::Principal::"cpetersen",
       action in [Dekopon::Action::"gh.content.read",
                  Dekopon::Action::"gh.pull-request.list",
                  Dekopon::Action::"gh.pull-request.read",
                  Dekopon::Action::"gh.pull-request.files",
                  Dekopon::Action::"gh.pull-request.approve"],
       resource == Dekopon::Provider::"gh")
when { context has agent && context.agent == "xaviers-rubber-stamper"
    && context has via && context.via == "dekopond-gateway" };
