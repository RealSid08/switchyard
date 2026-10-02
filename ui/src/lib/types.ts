// Wire types for the Switchyard admin API. Keep these aligned with docs/UI.md.

export type ConnectionKind = 'openai' | 'anthropic' | 'gemini' | 'codex';
export type Transport = 'http' | 'sse' | 'websocket';
export type RouteStrategy = 'round_robin' | 'failover';

export interface RequestRecord {
  id: string;
  /** ISO string or epoch (seconds or milliseconds). Normalise with `toDate`. */
  timestamp: string | number;
  model: string;
  connection_id: string | null;
  connection_name: string | null;
  transport: Transport;
  /** HTTP status. Usually a number, tolerated as a string. */
  status: number | string;
  latency_ms: number;
  input_tokens?: number | null;
  output_tokens?: number | null;
  error?: string | null;
}

export interface SeriesPoint {
  timestamp: string | number;
  requests: number;
  errors: number;
}

export interface Overview {
  version: string;
  uptime_seconds: number;
  paused: boolean;
  requests_total: number;
  requests_success: number;
  requests_failed: number;
  active_requests: number;
  connections_total: number;
  connections_enabled: number;
  latency_ms_p50: number | null;
  transport_counts: Record<Transport, number>;
  recent_requests: RequestRecord[];
  series: SeriesPoint[];
}

export interface Connection {
  id: string;
  name: string;
  kind: ConnectionKind;
  base_url: string;
  enabled: boolean;
  models: string[];
  supports_websocket: boolean;
  credential_present: boolean;
  created_at: string | number;
}

export interface ConnectionInput {
  name: string;
  kind: ConnectionKind;
  base_url: string;
  enabled: boolean;
  models: string[];
  supports_websocket: boolean;
  /** Omit to keep the stored credential on update. */
  api_key?: string;
}

export interface ConnectionTestResult {
  ok: boolean;
  status: number | null;
  latency_ms: number | null;
  message: string;
}

export type ImportSource = 'codex' | 'claude' | 'cliproxy';

export interface ImportResult {
  imported: number;
  connections: Connection[];
  message: string;
}

export interface ModelInfo {
  id: string;
  connection_id: string;
  connection_name: string;
  kind: ConnectionKind;
  supports_websocket: boolean;
}

export interface RouteTarget {
  connection_id: string;
  model: string;
}

export interface Route {
  model: string;
  targets: RouteTarget[];
  strategy: RouteStrategy;
}

export interface GatewayConfig {
  host: string;
  port: number;
  api_base: string;
  websocket_url: string;
  requires_api_key: boolean;
  max_in_flight: number;
  request_timeout_seconds: number;
}

export interface ApiKey {
  id: string;
  name: string;
  prefix: string;
  created_at: string | number;
}

export interface CreatedApiKey extends ApiKey {
  key: string;
}

export type EventMessage =
  | { type: 'request'; data: RequestRecord }
  | { type: 'overview'; data: Overview };
