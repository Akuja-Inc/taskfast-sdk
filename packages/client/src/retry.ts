export interface RetryOptions {
  maxAttempts: number;
  baseDelayMs: number;
}

export const DEFAULT_RETRY: RetryOptions = { maxAttempts: 3, baseDelayMs: 200 };

const RETRYABLE_STATUS_MIN = 500;
const RETRYABLE_STATUS_MAX = 599;

function shouldRetry(status: number): boolean {
  return status >= RETRYABLE_STATUS_MIN && status <= RETRYABLE_STATUS_MAX;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export function withRetry(
  base: typeof globalThis.fetch,
  opts: RetryOptions,
): typeof globalThis.fetch {
  return async (input, init) => {
    const asRequest = input instanceof Request ? input : null;
    for (let attempt = 1; attempt <= opts.maxAttempts; attempt += 1) {
      const toSend = asRequest ? asRequest.clone() : input;
      const res = await base(toSend, init);
      if (!shouldRetry(res.status) || attempt === opts.maxAttempts) return res;
      await sleep(opts.baseDelayMs * 2 ** (attempt - 1));
    }
    throw new Error("withRetry: unreachable");
  };
}
