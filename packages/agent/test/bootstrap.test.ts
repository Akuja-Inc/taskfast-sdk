import { describe, expect, it, vi } from "vitest";
import { validateAuth } from "../src/bootstrap.js";

describe("validateAuth", () => {
  it("returns the agent body when GET /agents/me succeeds", async () => {
    const agent = { id: "11111111-1111-1111-1111-111111111111", status: "active", name: "x" };
    const client = {
      GET: vi.fn().mockResolvedValue({ data: agent, error: undefined }),
    };
    await expect(validateAuth(client as never)).resolves.toBe(agent);
    expect(client.GET).toHaveBeenCalledWith("/agents/me", {});
  });

  it("throws when the underlying client returned an error envelope", async () => {
    const client = {
      GET: vi.fn().mockResolvedValue({ data: undefined, error: { error: "unauthorized" } }),
    };
    await expect(validateAuth(client as never)).rejects.toThrow(/validateAuth/);
  });
});
