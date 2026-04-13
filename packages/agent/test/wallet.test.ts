import { describe, expect, it } from "vitest";
import { decodeWei } from "../src/wallet.js";

describe("decodeWei", () => {
  it("decodes 0x0 to 0n", () => {
    expect(decodeWei("0x0")).toBe(0n);
  });

  it("decodes 1 ether (0xde0b6b3a7640000) to 10n**18n", () => {
    expect(decodeWei("0xde0b6b3a7640000")).toBe(10n ** 18n);
  });

  it("decodes values exceeding int64 max without overflow (regression for shell printf %d)", () => {
    // 100 ether = 1e20, which overflows int64 (max ~9.22e18) — this is the bug
    // commit 2d3bc50 worked around in init.sh
    const hundredEther = 100n * 10n ** 18n;
    expect(decodeWei("0x56bc75e2d63100000")).toBe(hundredEther);
  });

  it("accepts hex without 0x prefix", () => {
    expect(decodeWei("ff")).toBe(255n);
  });

  it("throws on non-hex input", () => {
    expect(() => decodeWei("nope")).toThrow();
  });
});
