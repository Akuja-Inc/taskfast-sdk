import { describe, expect, it, vi } from "vitest";
import { http, HttpResponse } from "msw";
import { createClient } from "../src/client.js";
import { AuthError, RateLimited, ServerError, ValidationError } from "../src/errors.js";
import { TEST_BASE_URL, use } from "./setup.js";

describe("createClient", () => {
  it("injects X-API-Key header on every request", async () => {
    const seen = vi.fn();
    use(
      http.get(`${TEST_BASE_URL}/api/agents/me`, ({ request }) => {
        seen(request.headers.get("x-api-key"));
        return HttpResponse.json({ id: "a", status: "active" });
      }),
    );
    const client = createClient({ baseUrl: TEST_BASE_URL, apiKey: "test-key" });
    await client.GET("/api/agents/me");
    expect(seen).toHaveBeenCalledWith("test-key");
  });

  it("throws AuthError on 401 carrying response body", async () => {
    use(
      http.get(`${TEST_BASE_URL}/api/agents/me`, () =>
        HttpResponse.json({ error: "unauthorized", message: "invalid key" }, { status: 401 }),
      ),
    );
    const client = createClient({ baseUrl: TEST_BASE_URL, apiKey: "bad" });
    await expect(client.GET("/api/agents/me")).rejects.toMatchObject({
      name: "AuthError",
      status: 401,
      body: { error: "unauthorized", message: "invalid key" },
    });
    await expect(client.GET("/api/agents/me")).rejects.toBeInstanceOf(AuthError);
  });

  it("throws ValidationError on 422 carrying server error_code", async () => {
    use(
      http.post(`${TEST_BASE_URL}/api/agents/me/wallet`, () =>
        HttpResponse.json(
          { error_code: "self_bidding", message: "cannot bid on own task" },
          { status: 422 },
        ),
      ),
    );
    const client = createClient({ baseUrl: TEST_BASE_URL, apiKey: "k" });
    await expect(
      client.POST("/api/agents/me/wallet", {
        body: { wallet_address: "0x0" } as never,
      }),
    ).rejects.toMatchObject({
      name: "ValidationError",
      status: 422,
      errorCode: "self_bidding",
    });
    await expect(
      client.POST("/api/agents/me/wallet", {
        body: { wallet_address: "0x0" } as never,
      }),
    ).rejects.toBeInstanceOf(ValidationError);
  });

  it("throws RateLimited on 429 carrying Retry-After seconds", async () => {
    use(
      http.get(`${TEST_BASE_URL}/api/agents/me`, () =>
        HttpResponse.json(
          { error: "rate_limited" },
          { status: 429, headers: { "Retry-After": "42" } },
        ),
      ),
    );
    const client = createClient({ baseUrl: TEST_BASE_URL, apiKey: "k" });
    await expect(client.GET("/api/agents/me")).rejects.toMatchObject({
      name: "RateLimited",
      status: 429,
      retryAfterSeconds: 42,
    });
    await expect(client.GET("/api/agents/me")).rejects.toBeInstanceOf(RateLimited);
  });

  it("retries 5xx with backoff and succeeds on recovery", async () => {
    let calls = 0;
    use(
      http.get(`${TEST_BASE_URL}/api/agents/me`, () => {
        calls += 1;
        if (calls < 3)
          return HttpResponse.json({ error: "boom" }, { status: 503 });
        return HttpResponse.json({ id: "a", status: "active" });
      }),
    );
    const client = createClient({
      baseUrl: TEST_BASE_URL,
      apiKey: "k",
      retry: { maxAttempts: 4, baseDelayMs: 1 },
    });
    const { data, error } = await client.GET("/api/agents/me");
    expect(error).toBeUndefined();
    expect(data).toMatchObject({ id: "a", status: "active" });
    expect(calls).toBe(3);
  });

  it("gives up after maxAttempts and throws ServerError", async () => {
    let calls = 0;
    use(
      http.get(`${TEST_BASE_URL}/api/agents/me`, () => {
        calls += 1;
        return HttpResponse.json({ error: "boom" }, { status: 503 });
      }),
    );
    const client = createClient({
      baseUrl: TEST_BASE_URL,
      apiKey: "k",
      retry: { maxAttempts: 3, baseDelayMs: 1 },
    });
    await expect(client.GET("/api/agents/me")).rejects.toMatchObject({
      name: "ServerError",
      status: 503,
    });
    await expect(client.GET("/api/agents/me")).rejects.toBeInstanceOf(ServerError);
    expect(calls).toBe(6);
  });

  it("RateLimited.retryAfterSeconds is undefined when header missing", async () => {
    use(
      http.get(`${TEST_BASE_URL}/api/agents/me`, () =>
        HttpResponse.json({ error: "rate_limited" }, { status: 429 }),
      ),
    );
    const client = createClient({ baseUrl: TEST_BASE_URL, apiKey: "k" });
    await expect(client.GET("/api/agents/me")).rejects.toMatchObject({
      name: "RateLimited",
      retryAfterSeconds: undefined,
    });
  });
});
