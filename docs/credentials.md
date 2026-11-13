# OpenAB Shepherd: credentials for the coding CLIs

Shepherd runs coding CLIs (Kiro, Devin, OpenCode) in terminal panes. Each CLI needs
an account credential before it works. This page records who provides it and how.
For the rest of the operator reference, see [operations.md](operations.md).
Tracking issue: [#15](https://github.com/openabdev/openab-shepherd/issues/15).

## The rule

> The operator does the one-time login or provides the key. Shepherd does not log in on the operator's behalf, and does not read, store, or log credentials, and does not pass them through argv.

In a pod, the operator provides credentials through a Kubernetes Secret (an
environment variable or a mounted file). On a developer machine, the CLI's existing
logged-in state is used.

## Providing credentials per CLI

| CLI | What the operator provides | Status |
|-----|----------------------------|--------|
| Kiro | An API key in an environment variable, shown here as `<KIRO_API_KEY>` | Documented approach |
| Devin | `credentials.toml`, written by one interactive login | See below |
| OpenCode | Not yet verified: the provider and the credential location are not confirmed | Not yet verified |

### Kiro

Set the key in the environment of the pane through a launch profile `env` entry that
reads from the pod's environment. The name of the variable is the one the Kiro CLI
documents. The value is always a placeholder here: `<KIRO_API_KEY>`.

### Devin

Log in once, interactively, as the operator. The CLI writes `credentials.toml`. Keep
that file as a Kubernetes Secret (`<DEVIN_CREDENTIALS_TOML>`) and mount it in the pod.
Per the Devin docs, the token does not expire by default.

`WINDSURF_API_KEY` is documented only for `devin acp`. It is not verified for any
other mode, so do not rely on it for panes.

### OpenCode

Not yet verified. Which provider will be used and where OpenCode stores its
credential have not been confirmed. Do not treat any OpenCode path or variable as
established until that is checked.

## Pod pattern

1. Create a Secret holding the credential. Never put it in the image or the repo.
2. Expose it to the pod in one of two ways:
   - as an environment variable (`<KIRO_API_KEY>`), or
   - as a mounted file. Secret mounts are read-only, so copy the file to a writable
     path in the home directory of the pod and set mode 600
     (`chmod 600 <path>/credentials.toml`).
3. Shepherd's launch profile passes only the named variables to the pane (see
   [operations.md](operations.md)).

## Shared identity

All panes act as one account. They share that account's quota. Revoking access means
logging in again as the operator and replacing the Secret. Shepherd cannot revoke or
rotate the credential for one pane.

## What Shepherd does and does not do

Shepherd does:
- start the CLI in a pane with a cleared child environment, plus HOME, PATH, USER,
  LANG, TERM and the extras named in the profile;
- show a blocked pane as blocked, with its `kind`.

Shepherd does not:
- log in for the operator;
- read, store or log credential values;
- pass credentials through argv;
- return env values from `list_panes`.

The name-based OpenCode auto-flag check is not a credential control. It only affects
which flags are added by name; it does not protect or restrict any credential.

## Verified and not verified

Checked on origin/main by reading code and tests only (no run). Details in
[#15](https://github.com/openabdev/openab-shepherd/issues/15).

Verified, env passthrough:
- The child environment is cleared, then set from HOME, PATH, USER, LANG, TERM and
  the profile's extras (`src/native/pty.rs`).
- A profile `env` is the only route for a credential. Names must be valid, at most
  32, and protected names are refused (`src/profiles.rs`).
- Panes started with a caller `argv` or `line` get no profile env
  (`src/native/mod.rs`).
- `list_panes` has no env field. `Secret` prints `<redacted>` in Debug output.
  Stderr output does not print env values.
- Gap: no test checks stderr for credential values.

Gap, `kind=auth` handling:
- A pane blocked with `kind=auth` gets the generic "blocked on a auth dialog" message,
  and `next` points the caller at `answer_dialog` (`src/tools.rs`).
- `answer_dialog` refuses any non-permission kind with the generic "not blocked on a
  permission dialog" text.
- There is no "operator must log in" message today, and no test checks the auth
  message text. Closing this gap is tracked in #15.

Not verified:
- OpenCode provider and credential location.
- `WINDSURF_API_KEY` outside `devin acp`.
- Live behavior of any CLI with a real login (nothing was run).
