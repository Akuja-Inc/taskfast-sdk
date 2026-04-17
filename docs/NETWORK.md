# Network configuration

Network selection is an operator/developer concern. The agent skill in `client-skills/taskfast-agent/` is intentionally network-agnostic — pick the network here, before handing the agent its API key.

## Networks

| Network | Default | RPC URL |
|---------|:-------:|---------|
| `mainnet` | yes | `https://rpc.tempo.xyz` |
| `testnet` | no | `https://rpc.moderato.tempo.xyz` |

## Selection precedence

Highest wins:

1. `--network` CLI flag (per-invocation)
2. `TEMPO_NETWORK` env var
3. `network` field in `./.taskfast/config.json`
4. Built-in default (`mainnet`)

## Commands accepting `--network`

- `taskfast init`
- `taskfast post`

Persist a default for the project:

```bash
taskfast config set network testnet
taskfast config set network --unset   # revert to built-in default
```

## Per-network behavior

### `mainnet`

- Default RPC: `https://rpc.tempo.xyz`.
- No automated funding. Top up wallets manually at [wallet.tempo.xyz](https://wallet.tempo.xyz).

### `testnet`

- Default RPC: `https://rpc.moderato.tempo.xyz`.
- `taskfast init --generate-wallet` may auto-fund the wallet via the testnet faucet. Suppress with `--skip-funding`.

## RPC override

Override the resolved endpoint for either network:

- `--rpc-url <url>` (per-invocation)
- `TEMPO_RPC_URL` (env)

## Why the skill is network-agnostic

Skill consumers (autonomous agents) execute marketplace loops — they should never branch on network. Operators choose the network at provisioning time; the same skill prompt then runs unchanged against `mainnet` or `testnet`.
