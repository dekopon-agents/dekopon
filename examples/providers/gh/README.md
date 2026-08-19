# gh provider

A "fake `gh`": nineteen separately named GitHub capabilities served by one Wasm component
over the broker-mediated `dekopon:http@1.0.0` import. Each capability is one fixed REST
request shape (or one fixed pre-read plus one write) with a strictly validated typed
input and a small bounded output projection — never a raw GitHub response.

## Capabilities

Read-only, `Low` risk, idempotent (one GET each unless noted):

| Capability | Reads |
|---|---|
| `gh.content.read` | One file (UTF-8 or base64, truncated with a flag) or directory listing at a path and optional ref |
| `gh.pull-request.list` | Pull requests, with state and post-pagination author filters |
| `gh.pull-request.read` | One pull request's metadata, state, and head/base SHAs |
| `gh.pull-request.files` | Changed files with bounded per-file patches |
| `gh.pull-request.diff` | The raw unified diff (`application/vnd.github.diff`), truncated with a flag |
| `gh.pull-request.reviews` | Existing reviews on one pull request |
| `gh.pull-request.status` | Head check runs (two GETs: pull, then check-runs) |
| `gh.repo.read` | Repository metadata: default branch, visibility, flags |
| `gh.branch.read` | One branch's head SHA and protection flag |
| `gh.commit.read` | One commit's message, author, stats, bounded file list |
| `gh.issue.read` / `gh.issue.list` / `gh.issue-comments.read` | Issues and their comments; pull requests hiding among issues are flagged |
| `gh.user.read` | One user's public profile |

External writes, each requiring its own broker grant:

| Capability | Risk | Writes |
|---|---|---|
| `gh.pull-request.approve` | High | An `APPROVE` review, SHA-pinned; refuses closed, merged, and draft pulls and moved heads |
| `gh.pull-request.comment` | Medium | A `COMMENT` review, SHA-pinned (drafts allowed) |
| `gh.pull-request.request-changes` | Medium | A `REQUEST_CHANGES` review, SHA-pinned, body required |
| `gh.pull-request.merge` | High | A merge (merge/squash/rebase), SHA-pinned; 405/409 map to `merge-conflict` |
| `gh.issue.comment` | Medium | One issue comment (non-idempotent) |

Every write pre-reads its pull request and pins the observed head SHA into the write
body (`commit_id` for reviews, `sha` for merge). A caller may additionally pass
`expectedHeadSha`; a mismatch refuses with `head-changed` before anything is written.
That pre-read is why the review and merge capabilities are classified `conditional`.

**There is deliberately no `gh.api.*` passthrough and no GraphQL.** Broker HTTP
constraints bind host and method, not path — a GET passthrough would turn one grant into
"everything the broker credential can read" while wearing a `read-only` label. Generic
fetch remains available as `http-probe.fetch` under its own explicit grant.

## Authority and credentials

The guest sets exactly `accept`, `x-github-api-version: 2022-11-28`, a constant
`user-agent`, and `content-type` on writes. It never sets `authorization` — the broker
host rejects guest credential headers by construction, and broker-owned destination-bound
credential injection is the only path a credential may take. A constraint set binds one by
symbolic name; the native HTTP engine adds the header after the guest's own headers were
validated, and audit records `credentialInjected: true` and never a value. A deployment that
binds no credential reaches only public data.
[`../../pr-summarizer-linter/`](../../pr-summarizer-linter/README.md) is the worked deployment:
six of these capabilities, one GitHub token, and a Slack DM that ends in one head-pinned review
comment while approval, change-request, and merge remain ungranted.

The endpoint defaults to `https://api.github.com` and otherwise accepts only a literal
loopback `http://` socket address, for deterministic tests. Transport failures are
reported as the constant `http-failed`; response bodies never appear in error messages.

Because the manifest declares external-write capabilities, the immediate `dekopon-run`
host refuses to load this component; it is broker-only, like `jsonplaceholder`.

`dekopon-shell`'s `gh` builtin is the model-facing half: `gh pr view 7 -R owner/repo` inside a
sandboxed script maps onto `gh.pull-request.read` and nothing else. `gh api` is refused by name.

## Regenerating

```console
./build.sh   # pinned wasm-tools 1.236.1; writes ../gh-provider.wasm
```

Native tests script exact request/response exchanges (no network):

```console
cargo fmt --manifest-path Cargo.toml -- --check
cargo clippy --locked --manifest-path Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path Cargo.toml
```
