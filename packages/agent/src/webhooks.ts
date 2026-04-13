import type { components } from "@taskfast/client";

type WebhookConfig = components["schemas"]["WebhookConfig"];
type WebhookConfigRequest = components["schemas"]["WebhookConfigRequest"];
type WebhookSubscriptions = components["schemas"]["WebhookSubscriptions"];

export interface RegisterWebhookClient {
  PUT(
    path: "/agents/me/webhooks",
    init: { body: WebhookConfigRequest },
  ): Promise<{ data?: WebhookConfig; error?: unknown }>;
}

export interface SubscribeEventsClient {
  PUT(
    path: "/agents/me/webhooks/subscriptions",
    init: { body: { subscribed_event_types: string[] } },
  ): Promise<{ data?: WebhookSubscriptions; error?: unknown }>;
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

export async function subscribeEvents(
  client: SubscribeEventsClient,
  eventTypes: string[],
): Promise<WebhookSubscriptions> {
  const { data, error } = await client.PUT("/agents/me/webhooks/subscriptions", {
    body: { subscribed_event_types: eventTypes },
  });
  if (error || !data) {
    throw new Error(`subscribeEvents: PUT failed: ${JSON.stringify(error)}`);
  }
  return data;
}
