// Wire types for the Switchyard admin API. Keep these aligned with docs/UI.md.

export type ConnectionKind = 'openai' | 'anthropic' | 'gemini' | 'codex' | 'antigravity';
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
  /** The route alias the client asked for, when it was a route (absent for direct model names). */
  route?: string | null;
  /** Times the request moved to a different account. */
  failovers?: number;
  /** Upstream attempts in order (bounded by the gateway). */
  attempts?: RequestAttempt[];
  /** Gateway-measured: request start (incl. earlier attempts) to first upstream body byte. */
  ttfb_ms?: number | null;
  /** Gateway-measured: request start to first non-empty output delta. */
  first_token_ms?: number | null;
}

/** Constant labels only; attempts never carry provider-supplied text. */
export type AttemptError =
  | 'credential_unavailable'
  | 'connect_failed'
  | 'transport_failed'
  | 'auth_rejected'
  | 'rate_limited'
  | 'provider_unavailable'
  | 'request_rejected';

export interface RequestAttempt {
  connection_id: string;
  connection_name: string;
  /** Model sent upstream, after route mapping. */
  model: string;
  /** Upstream HTTP status; 101 = WebSocket handshake, 0 = no HTTP response. */
  status: number;
  duration_ms: number;
  error: AttemptError | string | null;
}

export type HealthStatus = 'ready' | 'limited' | 'cooling' | 'disabled';

export interface ConnectionHealth {
  /** ready = not benched by Switchyard (not a network probe); limited = some models cooling; cooling = whole account ("*"). */
  status: HealthStatus;
  cooldowns: { model: string; retry_after_seconds: number }[];
  last_used_at: string | null;
  last_status: number | null;
  last_error: string | null;
}

export interface CatalogModel {
  id: string;
  name: string;
}

export interface ModelCatalog {
  connection_id: string;
  models: CatalogModel[];
  truncated?: boolean;
  message?: string;
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
  /** Who owns the credential. Absent on older gateways. */
  credential_source?: CredentialSource | string;
  /** Unix seconds when the stored credential expires, or null/absent if unknown/non-expiring. */
  credential_expires_at?: number | null;
  /** Absent on older gateways. */
  health?: ConnectionHealth;
  created_at: string | number;
}

/**
 * - api_key: pasted provider key
 * - native_codex / native_claude: imported CLI login; follows the CLI (never refreshed by Switchyard)
 * - cliproxy: imported CLIProxyAPI file; follows that file
 * - oauth: independent browser sign-in; Switchyard owns refresh
 */
export type CredentialSource = 'api_key' | 'native_codex' | 'native_claude' | 'cliproxy' | 'oauth';

export type OAuthProvider = 'codex' | 'claude' | 'antigravity';
export type OAuthStatus = 'pending' | 'complete' | 'error' | 'expired';

export interface OAuthFlow {
  id: string;
  provider: OAuthProvider;
  status: OAuthStatus;
  authorization_url?: string;
  expires_in_seconds?: number;
  connection?: Connection;
  message?: string;
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

export type ImportSource = 'codex' | 'claude' | 'cliproxy' | 'antigravity' | 'opencode' | 'opencode_go';

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
