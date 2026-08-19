// policies.cedar — who may do what, and through which gateway. Nothing here says how narrowly
// the broker executes it: timeouts, hosts, methods, call budgets, and credential binding live in
// broker.yaml's constraint sets. Editing policy may broaden who can act; it cannot reach a new
// host or bind authority that was not already constrained.
//
// chmod 0600, owner-owned, single-link. Cedar validates this in strict mode at startup against a
// schema generated from broker.yaml's declared world, so a typo in a principal, provider, or
// capability refuses startup instead of becoming a rule that silently never matches.
//
// The @id annotations are what audit records carry as `policy_ids`. Without them Cedar names
// policies positionally, which is unstable when a statement is inserted.

// Session gate: may this person drive this agent at all, and through which gateway? This is checked
// before `capabilitiesFor` answers, so an unauthorized sender is refused before a model call costs
// anything.
@id("maintainer-may-prompt-pr-summarizer-linter")
permit(principal == Dekopon::Principal::"maintainer",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"pr-summarizer-linter")
when { context has via && context.via == "dekopond-gateway" };

// Capability surface: driving the agent implies none of these actions, and holding these actions
// implies no session. `context.via` keeps attested and direct authority disjoint; `context.agent`
// pins the surface to this one agent. Approval, request-changes, and merge are deliberately absent.
@id("pr-summarizer-linter-gh-surface")
permit(principal == Dekopon::Principal::"maintainer",
       action in [Dekopon::Action::"gh.content.read",
                  Dekopon::Action::"gh.pull-request.read",
                  Dekopon::Action::"gh.pull-request.files",
                  Dekopon::Action::"gh.pull-request.diff",
                  Dekopon::Action::"gh.pull-request.status",
                  Dekopon::Action::"gh.pull-request.comment"],
       resource == Dekopon::Provider::"gh")
when { context has agent && context.agent == "pr-summarizer-linter"
    && context has via && context.via == "dekopond-gateway" };
