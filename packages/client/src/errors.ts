export class TaskFastError extends Error {
  readonly status: number;
  readonly body: unknown;
  constructor(name: string, status: number, body: unknown, message?: string) {
    super(message ?? `${name} (HTTP ${status})`);
    this.name = name;
    this.status = status;
    this.body = body;
  }
}

export class AuthError extends TaskFastError {
  constructor(status: number, body: unknown) {
    super("AuthError", status, body);
  }
}

export class ValidationError extends TaskFastError {
  readonly errorCode: string | undefined;
  constructor(status: number, body: unknown) {
    super("ValidationError", status, body);
    this.errorCode = extractErrorCode(body);
  }
}

export class ServerError extends TaskFastError {
  constructor(status: number, body: unknown) {
    super("ServerError", status, body);
  }
}

export class RateLimited extends TaskFastError {
  readonly retryAfterSeconds: number | undefined;
  constructor(status: number, body: unknown, retryAfterSeconds: number | undefined) {
    super("RateLimited", status, body);
    this.retryAfterSeconds = retryAfterSeconds;
  }
}

export function parseRetryAfter(header: string | null): number | undefined {
  if (!header) return undefined;
  const seconds = Number(header);
  if (Number.isFinite(seconds) && seconds >= 0) return seconds;
  const ms = Date.parse(header) - Date.now();
  return Number.isFinite(ms) && ms > 0 ? Math.ceil(ms / 1000) : undefined;
}

function extractErrorCode(body: unknown): string | undefined {
  if (body && typeof body === "object" && "error_code" in body) {
    const code = (body as { error_code: unknown }).error_code;
    return typeof code === "string" ? code : undefined;
  }
  return undefined;
}
