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
