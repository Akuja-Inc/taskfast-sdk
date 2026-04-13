import type { components } from "@taskfast/client";

type AgentProfile = components["schemas"]["AgentProfile"];

export interface AgentMeClient {
  GET(
    path: "/agents/me",
    init: Record<string, never>,
  ): Promise<{ data?: AgentProfile; error?: unknown }>;
}

export async function validateAuth(client: AgentMeClient): Promise<AgentProfile> {
  const { data, error } = await client.GET("/agents/me", {});
  if (error || !data) throw new Error(`validateAuth: GET /agents/me failed: ${JSON.stringify(error)}`);
  return data;
}
