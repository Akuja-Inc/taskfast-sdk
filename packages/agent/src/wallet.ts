import type { components } from "@taskfast/client";

type WalletBalance = components["schemas"]["WalletBalance"];

const HEX_RE = /^(0x)?[0-9a-fA-F]+$/;

export function decodeWei(hex: string): bigint {
  if (!HEX_RE.test(hex)) throw new Error(`decodeWei: not a hex string: ${hex}`);
  return BigInt(hex.startsWith("0x") ? hex : `0x${hex}`);
}

export interface WalletBalanceClient {
  GET(
    path: "/agents/me/wallet/balance",
    init: Record<string, never>,
  ): Promise<{ data?: WalletBalance; error?: unknown }>;
}

export interface PollBalanceOptions {
  minBalance: bigint;
  timeoutMs: number;
  pollIntervalMs: number;
  now?: () => number;
  sleep?: (ms: number) => Promise<void>;
}

export async function pollBalance(
  client: WalletBalanceClient,
  opts: PollBalanceOptions,
): Promise<bigint> {
  const now = opts.now ?? (() => Date.now());
  const sleep = opts.sleep ?? ((ms) => new Promise<void>((r) => setTimeout(r, ms)));
  const start = now();
  while (true) {
    const { data, error } = await client.GET("/agents/me/wallet/balance", {});
    if (error || !data?.available_balance) {
      throw new Error(`pollBalance: GET failed: ${JSON.stringify(error)}`);
    }
    const current = decodeWei(data.available_balance);
    if (current >= opts.minBalance) return current;
    if (now() - start >= opts.timeoutMs) {
      throw new Error(`pollBalance: timeout after ${opts.timeoutMs}ms (last=${current}, target=${opts.minBalance})`);
    }
    await sleep(opts.pollIntervalMs);
  }
}
