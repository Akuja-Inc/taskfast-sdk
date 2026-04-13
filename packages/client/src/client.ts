import createFetchClient, { type Client } from "openapi-fetch";
import { AuthError } from "./errors.js";
import type { paths } from "./schema.js";

export interface CreateClientOptions {
  baseUrl: string;
  apiKey: string;
  fetch?: typeof globalThis.fetch;
}

export function createClient(opts: CreateClientOptions): Client<paths> {
  const client = createFetchClient<paths>({
    baseUrl: opts.baseUrl,
    headers: { "X-API-Key": opts.apiKey },
    ...(opts.fetch ? { fetch: opts.fetch } : {}),
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
      return undefined;
    },
  });
  return client;
}
