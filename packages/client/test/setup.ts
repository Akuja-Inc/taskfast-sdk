import { afterAll, afterEach, beforeAll } from "vitest";
import { setupServer } from "msw/node";
import { type HttpHandler } from "msw";

export const TEST_BASE_URL = "http://taskfast.test";

const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

export function use(...handlers: HttpHandler[]): void {
  server.use(...handlers);
}
