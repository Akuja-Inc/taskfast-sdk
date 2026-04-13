import type { components } from "@taskfast/client";

type WebhookConfig = components["schemas"]["WebhookConfig"];
type WebhookConfigRequest = components["schemas"]["WebhookConfigRequest"];

export interface RegisterWebhookClient {
  PUT(
    path: "/agents/me/webhooks",
    init: { body: WebhookConfigRequest },
  ): Promise<{ data?: WebhookConfig; error?: unknown }>;
}

export async function registerWebhook(
  client: RegisterWebhookClient,
  body: WebhookConfigRequest,
): Promise<WebhookConfig> {
  const { data, error } = await client.PUT("/agents/me/webhooks", { body });
  if (error || !data) {
    throw new Error(`registerWebhook: PUT failed: ${JSON.stringify(error)}`);
  }
  return data;
}
