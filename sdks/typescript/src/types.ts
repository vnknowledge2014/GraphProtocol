export const GAP_MCP_PROTOCOL_VERSION = "2026-07-28" as const;

export type SearchKind = "operation" | "type" | "field" | "node_category";
export type TaskAction = "get" | "list" | "cancel" | "decide" | "submit";
export type TaskDecision = "approve" | "deny";
export type TaskState =
  | "submitted"
  | "working"
  | "input_required"
  | "completed"
  | "failed"
  | "canceled"
  | "rejected"
  | string;

export interface GapBudget {
  tokens?: number;
  ms?: number;
}

export interface GapRequestExtensions {
  deadlineMs?: number;
  budget?: GapBudget;
  includeTokensEstimate?: boolean;
  persistedQuery?: {
    version: 1;
    sha256Hash: string;
  };
}

export interface GraphqlErrorLocation {
  line: number;
  column: number;
}

export interface GraphqlErrorEntry {
  message: string;
  locations?: GraphqlErrorLocation[];
  path?: Array<string | number>;
  extensions?: Record<string, unknown>;
}

export interface GraphqlResponse<TData = Record<string, unknown>> {
  data?: TData | null;
  errors?: GraphqlErrorEntry[];
  extensions?: Record<string, unknown>;
}

export interface ExecuteOptions {
  operationName?: string;
  budget?: GapBudget;
  deadlineMs?: number;
  includeTokensEstimate?: boolean;
  headers?: Record<string, string>;
  signal?: AbortSignal;
}

export interface PersistedOperationEntry {
  name?: string;
  sha256Hash: string;
  document: string;
}

export interface McpToolAnnotations {
  readOnlyHint?: boolean;
  destructiveHint?: boolean;
  idempotentHint?: boolean;
  openWorldHint?: boolean;
}

export interface McpToolDefinition {
  name: string;
  description: string;
  annotations?: McpToolAnnotations;
  inputSchema: Record<string, unknown>;
}

export interface McpToolCallResult<TStructured = Record<string, unknown>> {
  content?: Array<{ type: string; text?: string; [key: string]: unknown }>;
  structuredContent?: TStructured;
  isError?: boolean;
  _meta?: Record<string, unknown>;
}

export interface GapSearchOptions {
  kinds?: SearchKind[];
  limit?: number;
  budgetTokens?: number;
  signal?: AbortSignal;
}

export interface GapDescribeTypesOptions {
  depth?: number;
  signal?: AbortSignal;
}

export interface GapTaskArgs {
  action: TaskAction;
  id?: string;
  decision?: TaskDecision;
  note?: string;
  input?: Record<string, unknown>;
}

export interface TaskRecord {
  id: string;
  state: TaskState;
  result?: unknown;
  error?: unknown;
  [key: string]: unknown;
}

export interface SseEventFrame<TData = Record<string, unknown>> {
  event: string;
  data?: GraphqlResponse<TData> | null;
  rawData?: string;
}

export interface GapClientOptions {
  baseUrl: string;
  token?: string;
  protocolVersion?: string;
  defaultHeaders?: Record<string, string>;
  fetchImpl?: typeof fetch;
}

export class GapSdkError extends Error {
  readonly status?: number;
  readonly code?: string;
  readonly details?: unknown;

  constructor(message: string, opts?: { status?: number; code?: string; details?: unknown }) {
    super(message);
    this.name = "GapSdkError";
    this.status = opts?.status;
    this.code = opts?.code;
    this.details = opts?.details;
  }
}
