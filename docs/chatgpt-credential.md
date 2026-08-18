# Deploying a ChatGPT subscription credential

`dekopon auth chatgpt login` runs OpenAI's device authorization flow: it prints a URL and a short
code and waits for a human to approve the login in a browser. Nothing in a pod can do that. A
containerized `dekopond` configured with `kind: chatgptSubscription` therefore cannot obtain a
credential on its own, and it never will — the flow is interactive by design.

This document is the whole lifecycle for getting a credential from a local login into a cluster and
keeping it correct afterwards. Read [`cli.md`](cli.md) for the command's contract, [`run.md`](run.md)
for the inference boundary, and [`dekopond.md`](dekopond.md) for the `models[].authFile` setting that
names the file in a pod.

**Status.** The `dekopon auth chatgpt export` command is **Current**. The chart-side seeding it
feeds is **Committed direction**: the requirement is specified below, and no Dekopon chart implements
it yet.

## What the credential actually is

Dekopon's credential file is a single JSON document holding an access token, a refresh token, an
expiry, and the ChatGPT account ID. Four properties of it decide the whole deployment shape.

**The refresh token rotates.** `refresh_credentials` posts `grant_type=refresh_token`, and
`request_token` rejects a response that omits either token, then builds a complete replacement
record. There is no path that keeps the old refresh token. Each refresh therefore invalidates its
predecessor, and any copy of the file taken before that refresh is dead.

**The rotated value must be persisted, or the turn fails.** `refresh_if_needed` refreshes when the
access token is inside sixty seconds of expiry, assigns the new record, and *returns*
`save_credentials`. A write failure is not a lost optimization that the next request retries; it is
an error that propagates out of the model call and fails the turn. A credential the process cannot
write back is a credential that stops working the first time its access token approaches expiry.

**Writing needs a writable directory, not a writable file.** `save_credentials` creates a sibling
temporary file in the credential file's own directory — `chatgpt-auth.tmp-<pid>` for the default
name — opens it `create_new` at mode `0600`, writes, `sync_all`s, and renames it over the target.
A `subPath` mount of a single file satisfies none of that: the rename needs a writable parent
directory, not merely a writable inode.

**Reading is unchecked.** `load_credentials` opens the path with a plain `File::open`. There is no
`O_NOFOLLOW`, no owner comparison, no mode check — deliberately unlike `dekopon-brokerd`, which
rejects its own configuration and credential files on all three counts. A symlink farm such as a
projected Secret volume is therefore perfectly readable. Reading was never the problem; writing is.

Put together: **a credential mounted read-only into a cluster breaks at the first refresh, and on
restart the pod would present a refresh token the provider has already invalidated.** The credential
has to reach a writable directory that survives a restart, and it has to get there exactly once.

## Export the credential

Sign in locally first, on a machine with a browser:

```console
dekopon auth chatgpt login
dekopon auth chatgpt status
```

`export` then reads the same credential file `login`, `status`, and `logout` use — `--auth-file`, then
`DEKOPON_CHATGPT_AUTH_FILE`, then `$XDG_CONFIG_HOME/dekopon/chatgpt-auth.json`, then
`$HOME/.config/dekopon/chatgpt-auth.json` — and prints it. It is the only Dekopon command whose
output is credential material in the clear, so it is gated twice and warns every time. The full
flag contract is in [`cli.md`](cli.md#exporting-a-credential-for-a-secret-store).

### The 1Password route

This is the production route when External Secrets Operator already runs against a 1Password vault.

```console
dekopon auth chatgpt export --expose-credential --format raw | pbcopy   # or your clipboard tool
```

`--format raw` emits exactly the bytes a login would have written, so one concealed field holds a
complete credential file and nothing has to be reassembled on the way out. Paste it into a single
field of a 1Password item; to script it instead, note that `op item edit` reads a JSON item template
from standard input and warns that command-line assignment statements are visible to other processes
on the machine. An `ExternalSecret` whose `remoteRef` names that item and field then projects it into
the namespace as a Secret key.

### The kubectl route

This is the quick route, and it puts the credential in the API server without a vault in between.

```console
dekopon auth chatgpt export --expose-credential --namespace dekopon | kubectl apply -f -
```

The default form is a `v1` `Secret` named `dekopon-chatgpt-auth` carrying the document under the key
`chatgpt-auth.json`, with a comment header repeating the rotation warning — because the manifest
outlives the terminal that warned. Base64 in `data` is Kubernetes' encoding for the field, not
protection.

## What the pod must do with it

**Not implemented.** No Dekopon chart performs this today. The requirement, precisely:

1. **Seed into a writable directory on durable storage.** The credential's directory must be on the
   persistent claim, not an `emptyDir`. An `emptyDir` is discarded when the pod is replaced, so a
   credential seeded there would be re-seeded on every reschedule — which is exactly the failure this
   design exists to prevent. `/var/lib/dekopon/chatgpt` alongside the audit chain is the natural
   home; `models[].authFile` in `dekopond.yaml` names the file inside it.

2. **The directory must be `0700` and owned by the runtime UID**, and the file `0600` and owned by
   the same UID. The directory permission is not hygiene theatre: `save_credentials` creates its
   temporary sibling there and renames over the target, so the daemon must be able to create files
   in that directory, not just rewrite one.

3. **Seed once, and never overwrite.** The init container must test for the destination before
   copying:

   ```sh
   install -d -m 0700 -o 65532 -g 65532 /var/lib/dekopon/chatgpt
   if [ ! -e /var/lib/dekopon/chatgpt/chatgpt-auth.json ]; then
     install -m 0600 -o 65532 -g 65532 \
       /dekopon-source/chatgpt-auth.json /var/lib/dekopon/chatgpt/chatgpt-auth.json
   fi
   ```

   The guard is the entire requirement. `install` overwrites unconditionally, so an unguarded copy
   would replace a credential the running daemon had already rotated with the older one from the
   vault, on every restart — and the vault copy is invalid the moment the daemon refreshes. Test
   `-e` rather than `-f` or `-s`, so any leftover at that path counts as seeded rather than being
   silently replaced.

4. **A changed source Secret must not re-seed.** Rolling the pod when the vault item changes is
   harmless in itself; overwriting the file when it rolls is not. Whatever triggers a restart, step 3
   still decides whether anything is written.

5. **Re-seeding must be a deliberate, separate act.** A value such as `reseed: true` that flips the
   init container to overwrite once is the right escape hatch, and it must document that using it
   against a running deployment discards a token fresher than the one in the vault.

6. **One writer only.** `ChatGptCodexModel` serializes refreshes behind a mutex within a process; it
   cannot coordinate across processes. Two pods sharing one credential file would race, and the loser
   would be left holding an invalidated refresh token. A deployment using this model kind must run
   exactly one replica, and must replace rather than overlap them on an update.

## The exported copy drifts, and that is expected

Once the pod refreshes, the credential in the vault or Secret is a dead token. Nothing detects this
and nothing repairs it — the exported copy and the live one are independent from the moment of
export.

That is fine, because the exported copy has exactly one job: to seed a *new* deployment. It is not a
backup, and restoring it over a live credential is a way to break a working pod, not to fix one.

When you do want to rotate deliberately — a new login, a revoked session, a fresh cluster — the
sequence is: log in locally again, re-export, update the vault item, delete the file in the volume,
and restart the pod. That is five steps because each one is a decision; none of them should happen
implicitly.

Revoking is separate: `dekopon auth chatgpt logout` deletes only Dekopon's local file. It does not
invalidate the exported copy, the Secret, the vault item, or the credential the pod is running on.
Revoke the session with OpenAI if that is what you need.
