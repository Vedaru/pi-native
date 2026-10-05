// pi-native session adapter for a pi-web fork.
//
// Drop this into pi-web's `lib/` and construct it in `startRpcSession`
// instead of `createAgentSessionFromServices`. It implements the subset of
// `AgentSessionLike` (lib/pi-types.ts) that the chat surface uses, backed by
// `pi-native --gateway`:
//
//   GET  /sessions/:id/events?format=pi  -> pi's canonical SSE event stream
//   POST /sessions/:id/commands          -> prompt/steer/abort/...
//   POST /sessions/:id/ui_response       -> answer a ui_request
//
// Events are already in pi's shapes (`agent_start`, `message_update`,
// `tool_execution_*`, `agent_settled`), so pi-web's existing client projection
// consumes them without translation.

export type PiNativeEvent = {
  type: string;
  [key: string]: unknown;
};

export type PiNativeListener = (event: PiNativeEvent) => void;

export interface PiNativeSessionOptions {
  /** Gateway origin, e.g. `http://127.0.0.1:30142`. */
  baseUrl: string;
  /** Session id returned by `POST /sessions`. */
  sessionId: string;
  /** Called when the SSE stream ends (network drop, host restart). */
  onDisconnect?: (reason: Error) => void;
}

/**
 * A live pi-native unit. Owns one SSE connection and its listener set.
 */
export class PiNativeSession {
  readonly sessionId: string;
  private readonly baseUrl: string;
  private readonly onDisconnect?: (reason: Error) => void;
  private listeners = new Set<PiNativeListener>();
  private controller: AbortController | null = null;
  private closed = false;
  private streaming = false;

  constructor(options: PiNativeSessionOptions) {
    this.baseUrl = options.baseUrl.replace(/\/$/, "");
    this.sessionId = options.sessionId;
    this.onDisconnect = options.onDisconnect;
  }

  // --- AgentSessionLike surface (live chat) ---------------------------------

  get isStreaming(): boolean {
    return this.streaming;
  }

  get isCompacting(): boolean {
    return false;
  }

  get autoCompactionEnabled(): boolean {
    return true;
  }

  get autoRetryEnabled(): boolean {
    return true;
  }

  get pendingMessageCount(): number {
    return 0;
  }

  subscribe(listener: PiNativeListener): () => void {
    this.listeners.add(listener);
    if (!this.controller) void this.openStream();
    return () => this.listeners.delete(listener);
  }

  async prompt(
    text: string,
    options?: {
      images?: Array<{ type: "image"; data: string; mimeType: string }>;
      streamingBehavior?: "steer" | "followUp";
      preflightResult?: (disposition: "handled" | "queued" | "started") => void;
    },
  ): Promise<void> {
    const command: Record<string, unknown> = { type: "prompt", text };
    if (options?.streamingBehavior) command.streamingBehavior = options.streamingBehavior;
    if (options?.images?.length) command.images = options.images;
    await this.send(command);
    // The unit acknowledges acceptance; completion arrives over SSE.
    options?.preflightResult?.("started");
  }

  async steer(text: string): Promise<"handled" | "queued"> {
    await this.send({ type: "steer", text });
    return "queued";
  }

  async followUp(text: string): Promise<"handled" | "queued"> {
    await this.send({ type: "follow_up", text });
    return "queued";
  }

  async abort(): Promise<void> {
    await this.send({ type: "abort" });
  }

  async compact(customInstructions?: string): Promise<unknown> {
    await this.send({ type: "compact", customInstructions });
    return null;
  }

  async executeBash(
    command: string,
    onChunk?: (chunk: string) => void,
    options?: { excludeFromContext?: boolean },
  ): Promise<{ output: string; exitCode?: number; cancelled?: boolean }> {
    await this.send({
      type: "bash",
      command,
      excludeFromContext: options?.excludeFromContext ?? false,
    });
    // The unit streams the command result as a `response` event; the caller can
    // read it from the event stream. For a synchronous call, poll state.
    onChunk?.("");
    return { output: "" };
  }

  async setModel(model: { provider: string; modelId: string }): Promise<void> {
    await this.send({ type: "set_model", provider: model.provider, modelId: model.modelId });
  }

  setThinkingLevel(level: string): void {
    void this.send({ type: "set_thinking_level", level });
  }

  setSessionName(name: string): void {
    void this.send({ type: "set_session_name", name });
  }

  setAutoCompactionEnabled(enabled: boolean): void {
    void this.send({ type: "set_auto_compaction", enabled });
  }

  setAutoRetryEnabled(enabled: boolean): void {
    void this.send({ type: "set_auto_retry", enabled });
  }

  clearQueue(): { steering: string[]; followUp: string[] } {
    void this.send({ type: "clear_queue" });
    return { steering: [], followUp: [] };
  }

  async reload(): Promise<void> {
    // Re-open the stream; the session itself is unchanged.
    this.controller?.abort();
    this.controller = null;
    if (!this.closed) void this.openStream();
  }

  dispose(): void {
    this.closed = true;
    this.controller?.abort();
    this.controller = null;
    this.listeners.clear();
  }

  // --- Internals ------------------------------------------------------------

  private async send(command: Record<string, unknown>): Promise<void> {
    const response = await fetch(
      `${this.baseUrl}/sessions/${encodeURIComponent(this.sessionId)}/commands`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(command),
      },
    );
    if (!response.ok) {
      throw new Error(`pi-native command failed: ${response.status} ${await response.text()}`);
    }
  }

  /** Answer a `ui_request` (approval/select/input). */
  async respondToUi(id: string, value: unknown): Promise<void> {
    const response = await fetch(
      `${this.baseUrl}/sessions/${encodeURIComponent(this.sessionId)}/ui_response`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id, value }),
      },
    );
    if (!response.ok) {
      throw new Error(`pi-native ui_response failed: ${response.status}`);
    }
  }

  private emit(event: PiNativeEvent): void {
    if (event.type === "agent_start") this.streaming = true;
    if (event.type === "agent_end" || event.type === "agent_settled") this.streaming = false;
    for (const listener of this.listeners) listener(event);
  }

  private async openStream(): Promise<void> {
    const controller = new AbortController();
    this.controller = controller;
    try {
      const response = await fetch(
        `${this.baseUrl}/sessions/${encodeURIComponent(this.sessionId)}/events?format=pi`,
        { headers: { Accept: "text/event-stream" }, signal: controller.signal },
      );
      if (!response.ok || !response.body) {
        throw new Error(`pi-native event stream failed: ${response.status}`);
      }
      await readSse(response.body, (event) => this.emit(event));
      if (!this.closed && !controller.signal.aborted) {
        throw new Error("pi-native event stream closed");
      }
    } catch (error) {
      if (!this.closed && !controller.signal.aborted) {
        this.onDisconnect?.(error instanceof Error ? error : new Error(String(error)));
      }
    } finally {
      if (this.controller === controller) this.controller = null;
    }
  }
}

/** Parse an SSE body, invoking `onEvent` for each `data:` payload. */
async function readSse(
  body: ReadableStream<Uint8Array>,
  onEvent: (event: PiNativeEvent) => void,
): Promise<void> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let index: number;
    while ((index = buffer.indexOf("\n\n")) !== -1) {
      const frame = buffer.slice(0, index);
      buffer = buffer.slice(index + 2);
      for (const line of frame.split("\n")) {
        if (!line.startsWith("data:")) continue;
        try {
          onEvent(JSON.parse(line.slice(5).trim()) as PiNativeEvent);
        } catch {
          // Ignore malformed frames; the stream stays open.
        }
      }
    }
  }
}

/** Open (or reuse) a gateway session and return its adapter. */
export async function openPiNativeSession(
  baseUrl: string,
  options: { sessionPath?: string },
): Promise<PiNativeSession> {
  const response = await fetch(`${baseUrl.replace(/\/$/, "")}/sessions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(options.sessionPath ? { sessionPath: options.sessionPath } : {}),
  });
  if (!response.ok) {
    throw new Error(`pi-native open session failed: ${response.status}`);
  }
  const { sessionId } = (await response.json()) as { sessionId: string };
  return new PiNativeSession({ baseUrl, sessionId });
}
