# Security Policy

## Reporting a Vulnerability

**Do not open a public GitHub issue.**

Report security vulnerabilities through GitHub's private vulnerability reporting:

1. Go to the [repository Security tab](https://github.com/Akuja-Inc/taskfast-cli/security).
2. Click **"Report a vulnerability"**.
3. Fill in the advisory details.

## Response SLA

| Stage | Target |
|-------|--------|
| Acknowledgment | 48 hours |
| Triage / severity assessment | 5 business days |
| Fix or mitigation | Depends on severity (critical: ASAP; low: next release) |

## Supported Versions

| Version | Supported |
|---------|-----------|
| `0.2.x` | Yes |
| `< 0.2.0` | No |

We support the latest minor release only. Pre-release versions (`0.1.x`, etc.)
are not eligible for security patches — upgrade to the latest version.

## Disclosure Policy

- Vulnerabilities are disclosed via GitHub Security Advisories after a fix is
  released.
- We request a 90-day embargo window for critical issues to allow users time to
  upgrade.
- Credit is given to reporters in the advisory unless anonymity is requested.

## Operator Guidance

This section covers the real-world ways a `taskfast` operator can be
compromised and how to avoid them. The CLI holds three classes of
secret — a TaskFast API key (PAT), a wallet private key (keystore +
password), and a webhook signing secret — and signs ERC-20 transfers
against a user-configurable RPC. The threats that actually matter are
misdirection and local exposure, not code bugs.

### Install

- Prefer `cargo install taskfast-cli --locked` **or** verify release
  attestations: `gh attestation verify taskfast-cli-*.tar.xz --repo
  Akuja-Inc/taskfast-cli` before extracting.
- Releases are also [cosign keyless-signed](#verifying-releases). Verify
  `SHA256SUMS` against `SHA256SUMS.sig` / `SHA256SUMS.pem` before running
  any binary pulled from a release.
- Pin a released version in automation; don't install from `main`.
- Docker: pull by digest, not tag.

#### Verifying releases

```bash
cosign verify-blob \
  --certificate-identity-regexp '^https://github.com/Akuja-Inc/taskfast-cli/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --signature  SHA256SUMS.sig \
  --certificate SHA256SUMS.pem \
  SHA256SUMS
```

### Secrets hygiene

- **Never commit `./.taskfast/`**. Add it to every project `.gitignore` —
  the directory holds your PAT.
- **Never run `taskfast` in a CWD you don't trust.** A repo-bundled
  `.taskfast/config.json` can redirect `api_base` and `tempo_rpc_url` to
  attacker infra. Since v0.3 the CLI hard-refuses non-well-known
  endpoints without `--allow-custom-endpoints`; a warning is emitted in
  the envelope's `security_warnings[]` when the flag is in effect.
- Prefer `--wallet-password-file` (mode `0600`) over the
  `TASKFAST_WALLET_PASSWORD` env var — env vars leak via
  `/proc/<pid>/environ`, `ps e`, and crash dumps. A warning is emitted
  in the envelope when the env-var path is active.
- Keystore and password file belong on separate paths where possible;
  back the keystore up offline (hardware / paper / sealed USB).
- Rotate the TaskFast PAT quarterly; revoke it at the first suspicious
  event.
- `taskfast config show` redacts by default; **never paste `--reveal`
  output into tickets, Slack, or LLM chats**.
- Avoid running with `--verbose` and then sharing the stderr transcript
  unscrubbed.

### Wallet safety

- Use a **dedicated hot wallet** funded with only the working capital
  you're willing to lose. Treasury funds stay in a cold wallet.
- Before approving a task post, confirm the fee destination shown in
  the CLI's stderr audit line matches the published TaskFast treasury
  address on the correct network (`4217` mainnet / `42431` Moderato
  testnet). The CLI also refuses to sign when the server returns a
  non-PathUSD `token_address`.
- The CLI serializes sign+broadcast per keystore via a file lock
  (`<keystore-dir>/.taskfast-wallet.lock`), so multiple `taskfast post`
  invocations against the same wallet on the **same host** are safe.
  Multi-host farms pointing at the same wallet still need external
  nonce coordination.

### Network

- Pin `tempo_rpc_url` to a well-known HTTPS endpoint. Plain-HTTP RPC on
  `--network=mainnet` is refused unless the host is loopback.
- If you override `api_base`, expect auth tokens to flow there. Only do
  it pointing at `localhost` during development, and do so knowing the
  `security_warnings[]` array in your envelope will flag it.

### Webhook

- The signing secret is shown **once**. Store it in your secret manager,
  not a config repo. File mode must be `0600`; verify with `stat`. The
  CLI writes the file atomically (tmp → chmod → rename) so a concurrent
  reader can never observe a looser mode.
- In your webhook handler, always (a) check the timestamp is within 5
  min, (b) HMAC-verify with a constant-time compare, (c) reject replay
  by `delivery_id`.

### Supply chain

- Treat the `curl | sh` installer as equivalent to running an arbitrary
  binary with your UID. If that isn't acceptable, use `cargo install
  --locked` or Homebrew.
- The published Docker image runs as an unprivileged `taskfast` (uid
  1000) user; still run with `--read-only --cap-drop=ALL` for defense
  in depth.

### Incident response

- **PAT leak.** Rotate at the TaskFast dashboard, then `taskfast config
  set api_key --unset` + `taskfast init` again.
- **Keystore/password leak.** Move funds first, then generate a new
  wallet via `taskfast init --generate-wallet` on a clean host.
- **Suspicious stderr / unknown `api_base`.** Stop the CLI before it
  issues more requests; `cat .taskfast/config.json` and check the
  envelope's `security_warnings[]` for the signal.
