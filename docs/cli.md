# Auth CLI reference

`dekopond auth chatgpt {login,status,logout,export}` manages Dekopon's isolated model credential.
It dispatches synchronously before gateway configuration discovery, telemetry, runtime creation,
or transport startup. `--config` is required only for ordinary gateway serving and is ignored by auth.
The standalone catalog CLI has been retired; catalogs remain loaded and validated by `dekopon-config`.
The runner remains documented in [`run.md`](run.md), and daemon serving in [`dekopond.md`](dekopond.md).

## Commands

```console
dekopond auth chatgpt login
dekopond auth chatgpt status
dekopond auth chatgpt logout
dekopond auth chatgpt export --expose-credential
```

## Auth flags

- `-o, --output <FORMAT>`: `table` (default), `wide`, `json`, `yaml`, or `name`.
- `--no-color`: disable ANSI color in diagnostics.
- `--quiet`: suppress successful output; errors still print. Conflicts with `-v`.
- `-v`: emit informational diagnostics and error causes.
- `-vv`: emit debug diagnostics and debug error context.

Auth flags may appear anywhere after `auth`. Authentication commands do not load the catalog. `--output json` or `--output yaml` keeps authentication status machine-readable; device-login instructions are written to standard error so standard output remains parseable. `--output` does not apply to `auth chatgpt export`, whose form is chosen by `--format`.

Credential-file parse failures never print credential-file values, including at `-v` and `-vv`.
Verbose diagnostics retain the safe JSON error category and line/column (and I/O kind when
available); debug output identifies the typed parse failure without rendering its error tree.
Non-parse failures retain their error causes and additional debug context.

## ChatGPT subscription authentication

`dekopond auth chatgpt login` uses OpenAI's Codex device authorization flow and writes only to Dekopon's credential file. `status` reports state without revealing tokens, and `logout` removes only Dekopon's file. The credential file is resolved in this exact order: `--auth-file <PATH>`, `DEKOPON_CHATGPT_AUTH_FILE`, `$XDG_CONFIG_HOME/dekopon/chatgpt-auth.json`, `$HOME/.config/dekopon/chatgpt-auth.json`, then `%APPDATA%/dekopon/chatgpt-auth.json`; when no tier applies the command fails asking for `DEKOPON_CHATGPT_AUTH_FILE`. Discovery treats a variable exported with an empty value as unset and falls through to the next tier, and refuses a discovered path that is not absolute — a relative `DEKOPON_CHATGPT_AUTH_FILE` or `XDG_CONFIG_HOME` would otherwise leave the rotating refresh token in whatever directory the process started in. Only `--auth-file` is taken verbatim. See [`run.md`](run.md) for inference behavior and the complete security boundary.

### Exporting a credential for a secret store

`dekopond auth chatgpt export` prints an existing local credential so it can be seeded into a secret store. It exists because device authorization needs a human at a browser: a pod can only ever run on a credential an operator carried out of a local login. It resolves the credential file exactly as `login`, `status`, and `logout` do, including `--auth-file`.

**This is the one Dekopon command whose output is credential material in the clear.** Everywhere else a credential renders a redaction marker. Two gates and a warning make that deliberate rather than incidental:

- `--expose-credential` is required. It has no default and no short form, so exporting is something an operator typed, and it is greppable in a shell history or a runbook.
- Standard output is refused when it is a terminal, because intent does not cover destination: an operator who means to export still should not leave a live refresh token in scrollback, a `tmux` capture, or a screen share. Every intended consumer is a pipe or a redirect. `--allow-terminal` overrides it.
- Both forms warn on standard error that the copy is stale the moment the live credential refreshes, and the Secret manifest repeats that in a comment header, because the manifest outlives the terminal.

| Flag | Meaning |
|---|---|
| `--format secret` | Default. A `v1` `Secret` manifest for `kubectl apply -f -`. |
| `--format raw` | The credential document itself, byte-identical to what a login writes, for a password-manager field. |
| `--secret-name <NAME>` | Secret name, default `dekopon-chatgpt-auth`; validated as an RFC 1123 subdomain before the credential is read. |
| `--namespace <NAMESPACE>` | Secret namespace; validated as an RFC 1123 label (no dots, at most 63 characters) before the credential is read; omitted from the manifest when unset. |
| `--expose-credential` | Required acknowledgement that this prints a live access token and refresh token. |
| `--allow-terminal` | Print to a terminal anyway. |

`--quiet` is refused, because suppressing the document while exiting `0` is how a scripted seeding step stores nothing and believes it succeeded.

The manifest carries the document under the key `chatgpt-auth.json`, matching Dekopon's own file name. Missing, malformed, incomplete, and unsupported-version credential files all fail with exit code `1` and print nothing, so a seeding step never stores a half-formed secret.

The refresh token rotates, so an exported copy is invalidated by the next refresh of the credential it came from. [`chatgpt-credential.md`](chatgpt-credential.md) is the full deployment lifecycle: export, store, seed once into a writable directory, and re-export only on a deliberate rotation.

## Output behavior

Status emits `{account, credentialFile, signedIn, expired}` in JSON/YAML, `auth/chatgpt` in
name format, or an account/status/credential-file table in table and wide formats. Table cells
remove terminal controls. Login and logout render the resulting status. Export ignores `--output`
and writes only the chosen document. Device instructions and export warnings go to stderr.
BrokenPipe on stdout is successful termination.

## Exit codes

| Code | Meaning |
|---:|---|
| `0` | Success |
| `1` | Credential, rendering, or runtime failure |
| `2` | Command-line usage error |
