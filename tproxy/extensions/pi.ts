/**
 * tproxy provider for Pi.
 *
 * Registers the local tproxy endpoint as an OpenAI-compatible provider.
 * Model IDs, names, capabilities, limits, and costs come from GET /v1/models.
 *
 * Usage:
 *   pi -e ./extensions/pi.ts
 *   /login tproxy
 *
 * The endpoint defaults to http://127.0.0.1:8001/v1. Override it in
 * models.json if needed.
 */

import type {
  ExtensionAPI,
  ProviderConfig,
  ProviderModelConfig,
} from "@earendil-works/pi-coding-agent";

const PROVIDER = "tproxy";
const BASE_URL = "http://127.0.0.1:8001/v1";
const TIMEOUT_MS = 5000;

interface RemoteModel {
  id: string;
  name: string;
  reasoning: boolean;
  input: ("text" | "image")[];
  cost: {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
  };
  contextWindow: number;
  maxTokens: number;
}

function normalizeBaseUrl(url: string): string {
  const value = url.replace(/\/+$/, "");
  return value.endsWith("/v1") ? value : `${value}/v1`;
}

const positive = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value) && value > 0;

const nonNegative = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value) && value >= 0;

function toModelConfig(value: unknown): ProviderModelConfig {
  const model = value as Partial<RemoteModel> | null;
  const cost = model?.cost as Partial<RemoteModel["cost"]> | undefined;
  if (
    !model ||
    typeof model.id !== "string" ||
    !model.id ||
    typeof model.name !== "string" ||
    !model.name ||
    typeof model.reasoning !== "boolean" ||
    !Array.isArray(model.input) ||
    !model.input.every((input) => input === "text" || input === "image") ||
    !positive(model.contextWindow) ||
    !positive(model.maxTokens) ||
    !cost ||
    !nonNegative(cost.input) ||
    !nonNegative(cost.output) ||
    !nonNegative(cost.cacheRead) ||
    !nonNegative(cost.cacheWrite)
  ) {
    throw new Error("GET /models returned incomplete model metadata");
  }
  return { ...(model as RemoteModel), type: "chat" };
}

async function discoverModels(
  url: string,
  apiKey: string,
  signal: AbortSignal,
): Promise<ProviderModelConfig[]> {
  const response = await fetch(`${url}/models`, {
    headers: { accept: "application/json", authorization: `Bearer ${apiKey}` },
    signal: AbortSignal.any([signal, AbortSignal.timeout(TIMEOUT_MS)]),
  });
  if (!response.ok) {
    throw new Error(`GET /models returned ${response.status}`);
  }
  const payload = (await response.json()) as { data?: unknown } | null;
  if (!payload || !Array.isArray(payload.data)) {
    throw new Error("GET /models returned an invalid response");
  }
  return payload.data.map(toModelConfig);
}

export default function (pi: ExtensionAPI) {
  const config: ProviderConfig = {
    name: "tproxy",
    baseUrl: BASE_URL,
    api: "openai-completions",
    async refreshModels(context) {
      if (!context.allowNetwork) {
        throw new Error("tproxy model discovery requires network access");
      }
      const apiKey =
        context.credential?.type === "oauth"
          ? context.credential.access
          : context.credential?.key;
      if (!apiKey) {
        throw new Error("Run /login tproxy first");
      }
      const models = await discoverModels(
        normalizeBaseUrl(context.baseUrl ?? BASE_URL),
        apiKey,
        context.signal,
      );
      if (models.length === 0) {
        throw new Error("tproxy returned no models");
      }
      return models;
    },
  };

  pi.registerProvider(PROVIDER, config);
}
