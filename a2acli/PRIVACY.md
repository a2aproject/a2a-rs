# a2acli Privacy Policy

`a2acli` is a command-line A2A protocol client. It has no server component, no
telemetry, and no vendor operated by the a2aproject collects any data from
running it. Everything below describes what the tool itself reads and where
it sends it — the rest of the data's handling is determined by whichever A2A
endpoint you point it at, which is your choice on every invocation.

## What it reads locally

- Command-line flags and arguments.
- Environment variables prefixed `A2ACLI_`.
- A local `.env` file (found by walking up from the working directory, or a
  path you pass explicitly) and a global `.env` file at
  `~/.config/a2a-cli/.env`. `a2acli` never writes to either file; it only
  reads them, and warns if one it reads is more widely readable than it
  should be.
- Files you pass as message parts, task payloads, or push-notification
  configuration.

## What it sends, and to whom

Every network call goes only to the A2A agent endpoint you configure —
never to any address chosen by `a2acli` itself. A single invocation may send:

- Message text, file contents, and task data you supply, to that endpoint.
- Credentials you configured (a bearer token, an API key, a
  `--svc-param` value, or push-notification authentication) as request
  headers or fields, to that same endpoint and to any webhook URL you
  register for push notifications.

What that endpoint or webhook does with what it receives — how long it is
kept, whether it is logged, and whether it is shared further — is up to the
agent you selected, not `a2acli`. Review the privacy practices of any agent
you connect to before sending it sensitive data.

## Credential and log handling

Credentials are held in memory only for the duration of a command and are
never written anywhere by `a2acli` itself (it does not create or modify
`.env` files). The `config show` command and `--debug` wire logging both
redact credential values by construction — a credential is shown as
`(redacted)` or `authorization: (redacted)` rather than in the clear — and
this cannot be disabled by any flag or environment variable.

## Telemetry

`a2acli` does not collect usage analytics, crash reports, or any other
telemetry, and makes no network calls other than the ones you direct it to
make against your configured A2A endpoint.

## Changes to this policy

This file is versioned in the `a2a-rs` repository alongside the tool itself;
its history is visible via normal git history on that path.

## Contact

Report a concern via the issue tracker:
<https://github.com/a2aproject/a2a-rs/issues>.
