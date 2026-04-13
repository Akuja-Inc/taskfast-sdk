import { describe, expect, it, vi } from "vitest";
import { getReadiness } from "../src/bootstrap.js";

describe("getReadiness", () => {
  it("returns the readiness payload", async () => {
    const readiness = {
      ready_to_work: true,
      checks: {
        api_key: { status: "complete" },
        wallet: { status: "complete" },
        webhook: { status: "configured", required: false },
      },
    };
    const client = { GET: vi.fn().mockResolvedValue({ data: readiness, error: undefined }) };
    await expect(getReadiness(client as never)).resolves.toBe(readiness);
    expect(client.GET).toHaveBeenCalledWith("/agents/me/readiness", {});
  });

  it("throws on error envelope", async () => {
    const client = {
      GET: vi.fn().mockResolvedValue({ data: undefined, error: { error: "unauthorized" } }),
    };
    await expect(getReadiness(client as never)).rejects.toThrow(/getReadiness/);
  });
});
