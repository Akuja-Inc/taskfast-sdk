import { describe, expect, it, vi } from "vitest";
import { registerWebhook, subscribeEvents } from "../src/webhooks.js";

describe("registerWebhook", () => {
  it("PUTs /agents/me/webhooks and returns config carrying secret on first registration", async () => {
    const config = {
      url: "https://hook.example/agent",
      secret: "whsec_first_time_only",
      events: null,
      created_at: "2026-04-13T00:00:00Z",
      updated_at: "2026-04-13T00:00:00Z",
    };
    const client = { PUT: vi.fn().mockResolvedValue({ data: config, error: undefined }) };
    const result = await registerWebhook(client as never, { url: config.url });
    expect(result).toBe(config);
    expect(client.PUT).toHaveBeenCalledWith("/agents/me/webhooks", { body: { url: config.url } });
  });

  it("forwards events list when provided", async () => {
    const client = {
      PUT: vi.fn().mockResolvedValue({
        data: { url: "u", secret: "s", events: ["task_assigned"], created_at: "", updated_at: "" },
        error: undefined,
      }),
    };
    await registerWebhook(client as never, { url: "u", events: ["task_assigned"] });
    expect(client.PUT).toHaveBeenCalledWith("/agents/me/webhooks", {
      body: { url: "u", events: ["task_assigned"] },
    });
  });

  it("throws on error envelope", async () => {
    const client = {
      PUT: vi.fn().mockResolvedValue({ data: undefined, error: { error: "validation_failed" } }),
    };
    await expect(registerWebhook(client as never, { url: "bad" })).rejects.toThrow(/registerWebhook/);
  });
});

describe("subscribeEvents", () => {
  it("PUTs subscriptions and returns the resulting list", async () => {
    const response = {
      subscribed_event_types: ["task_assigned", "payment_disbursed"],
      available_event_types: ["task_assigned", "payment_disbursed", "test"],
    };
    const client = { PUT: vi.fn().mockResolvedValue({ data: response, error: undefined }) };
    const result = await subscribeEvents(client as never, ["task_assigned", "payment_disbursed"]);
    expect(result).toBe(response);
    expect(client.PUT).toHaveBeenCalledWith("/agents/me/webhooks/subscriptions", {
      body: { subscribed_event_types: ["task_assigned", "payment_disbursed"] },
    });
  });

  it("throws on error envelope", async () => {
    const client = {
      PUT: vi.fn().mockResolvedValue({ data: undefined, error: { error: "unknown_event_types" } }),
    };
    await expect(subscribeEvents(client as never, ["bogus"])).rejects.toThrow(/subscribeEvents/);
  });
});
