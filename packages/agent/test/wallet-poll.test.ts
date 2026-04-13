import { describe, expect, it, vi } from "vitest";
import { pollBalance } from "../src/wallet.js";

function balance(hex: string): { data: { available_balance: string }; error: undefined } {
  return { data: { available_balance: hex }, error: undefined };
}

describe("pollBalance", () => {
  it("returns immediately when first response is at or above target", async () => {
    const client = {
      GET: vi.fn().mockResolvedValueOnce(balance("0xa")),
    };
    const sleep = vi.fn().mockResolvedValue(undefined);
    const result = await pollBalance(client as never, {
      minBalance: 5n,
      timeoutMs: 60_000,
      pollIntervalMs: 100,
      now: () => 0,
      sleep,
    });
    expect(result).toBe(10n);
    expect(client.GET).toHaveBeenCalledTimes(1);
    expect(sleep).not.toHaveBeenCalled();
  });

  it("polls until balance reaches target", async () => {
    const client = {
      GET: vi
        .fn()
        .mockResolvedValueOnce(balance("0x0"))
        .mockResolvedValueOnce(balance("0x1"))
        .mockResolvedValueOnce(balance("0xa")),
    };
    const sleep = vi.fn().mockResolvedValue(undefined);
    const result = await pollBalance(client as never, {
      minBalance: 5n,
      timeoutMs: 60_000,
      pollIntervalMs: 250,
      now: () => 0,
      sleep,
    });
    expect(result).toBe(10n);
    expect(client.GET).toHaveBeenCalledTimes(3);
    expect(sleep).toHaveBeenCalledWith(250);
  });

  it("throws when timeout elapses before target reached", async () => {
    const client = {
      GET: vi.fn().mockResolvedValue(balance("0x0")),
    };
    let t = 0;
    const sleep = vi.fn(async (ms: number) => { t += ms; });
    await expect(
      pollBalance(client as never, {
        minBalance: 5n,
        timeoutMs: 500,
        pollIntervalMs: 200,
        now: () => t,
        sleep,
      }),
    ).rejects.toThrow(/timeout/i);
  });
});
