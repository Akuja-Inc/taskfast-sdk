import createFetchClient, { type Client } from "openapi-fetch";
import {
  AuthError,
  parseRetryAfter,
  RateLimited,
  ServerError,
  ValidationError,
} from "./errors.js";
import { DEFAULT_RETRY, type RetryOptions, withRetry } from "./retry.js";
import type { paths } from "./schema.js";

export interface CreateClientOptions {
  baseUrl: string;
  apiKey: string;
  fetch?: typeof globalThis.fetch;
  retry?: RetryOptions;
}

export function createClient(opts: CreateClientOptions): Client<paths> {
  const retry = opts.retry ?? DEFAULT_RETRY;
  const baseFetch = opts.fetch ?? globalThis.fetch;
  const wrappedFetch = withRetry(baseFetch, retry);
  const client = createFetchClient<paths>({
    baseUrl: opts.baseUrl,
    headers: { "X-API-Key": opts.apiKey },
    fetch: wrappedFetch,
  });
  client.use({
    async onResponse({ response }) {
      if (response.ok) return undefined;
      const body = await response
        .clone()
        .json()
        .catch(() => null);
      if (response.status === 401 || response.status === 403) {
        throw new AuthError(response.status, body);
      }
      if (response.status === 422) {
        throw new ValidationError(response.status, body);
      }
      if (response.status === 429) {
        throw new RateLimited(
          response.status,
          body,
          parseRetryAfter(response.headers.get("retry-after")),
        );
      }
      if (response.status >= 500) {
        throw new ServerError(response.status, body);
      }
      return undefined;
    },
  });
  return client;
}
