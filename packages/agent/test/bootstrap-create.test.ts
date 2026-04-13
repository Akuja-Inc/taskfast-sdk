import { describe, expect, it, vi } from "vitest";
import { createAgentHeadless } from "../src/bootstrap.js";

describe("createAgentHeadless", () => {
  it("POSTs /agents and returns response with api_key", async () => {
    const response = {
      id: "22222222-2222-2222-2222-222222222222",
      account_id: "33333333-3333-3333-3333-333333333333",
      api_key: "am_live_secret",
      name: "bot",
      status: "active",
    };
    const client = {
      POST: vi.fn().mockResolvedValue({ data: response, error: undefined }),
    };
    const body = {
      owner_id: "44444444-4444-4444-4444-444444444444",
      name: "bot",
      description: "test bot",
      capabilities: ["research"],
    };
    await expect(createAgentHeadless(client as never, body)).resolves.toBe(response);
    expect(client.POST).toHaveBeenCalledWith("/agents", { body });
  });

  it("throws when api_key is missing in the response", async () => {
    const client = {
      POST: vi
        .fn()
        .mockResolvedValue({ data: { id: "x", status: "active" }, error: undefined }),
    };
    const body = {
      owner_id: "44444444-4444-4444-4444-444444444444",
      name: "bot",
      description: "test bot",
      capabilities: ["research"],
    };
    await expect(createAgentHeadless(client as never, body)).rejects.toThrow(/api_key/);
  });
});
