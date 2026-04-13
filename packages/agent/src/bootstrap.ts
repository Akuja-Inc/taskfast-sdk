import type { components } from "@taskfast/client";

type AgentProfile = components["schemas"]["AgentProfile"];
type AgentCreateRequest = components["schemas"]["AgentCreateRequest"];
type AgentCreateResponse = components["schemas"]["AgentCreateResponse"];
type AgentReadiness = components["schemas"]["AgentReadiness"];

export interface AgentMeClient {
  GET(
    path: "/agents/me",
    init: Record<string, never>,
  ): Promise<{ data?: AgentProfile; error?: unknown }>;
}

export interface RegisterAgentClient {
  POST(
    path: "/agents",
    init: { body: AgentCreateRequest },
  ): Promise<{ data?: AgentCreateResponse; error?: unknown }>;
}

export interface ReadinessClient {
  GET(
    path: "/agents/me/readiness",
    init: Record<string, never>,
  ): Promise<{ data?: AgentReadiness; error?: unknown }>;
}

export async function getReadiness(client: ReadinessClient): Promise<AgentReadiness> {
  const { data, error } = await client.GET("/agents/me/readiness", {});
  if (error || !data) {
    throw new Error(`getReadiness: GET /agents/me/readiness failed: ${JSON.stringify(error)}`);
  }
  return data;
}

export async function validateAuth(client: AgentMeClient): Promise<AgentProfile> {
  const { data, error } = await client.GET("/agents/me", {});
  if (error || !data) throw new Error(`validateAuth: GET /agents/me failed: ${JSON.stringify(error)}`);
  return data;
}

export async function createAgentHeadless(
  client: RegisterAgentClient,
  body: AgentCreateRequest,
): Promise<AgentCreateResponse> {
  const { data, error } = await client.POST("/agents", { body });
  if (error || !data) {
    throw new Error(`createAgentHeadless: POST /agents failed: ${JSON.stringify(error)}`);
  }
  if (!data.api_key) {
    throw new Error("createAgentHeadless: response missing api_key — cannot persist credentials");
  }
  return data;
}
