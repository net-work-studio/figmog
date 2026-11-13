// Browser side of the figmog bridge (NWS-2957). Works only in a page served
// by a Vite dev server running the figmog plugin, which injects
// `window.__FIGMOG_BRIDGE__`; in a production build it is absent and
// `bridgeAvailable()` is false.

/** What the dev server injects into its page. */
export interface BridgeInfo {
  token: string;
  url: string;
}

/** An MCP `CallToolResult`, as figmog returns it. */
export interface CallToolResult {
  content: ({ type: "text"; text: string } | { type: "image"; data: string; mimeType: string })[];
  isError?: boolean;
}

function info(): BridgeInfo | null {
  const value = (globalThis as { __FIGMOG_BRIDGE__?: BridgeInfo }).__FIGMOG_BRIDGE__;
  return value === undefined ? null : value;
}

/** Whether this page can reach the figmog bridge. */
export function bridgeAvailable(): boolean {
  return info() !== null;
}

/**
 * Calls a figmog read tool (or `figmog_images`) through the dev server's
 * bridge and resolves with its MCP result. Rejects when the bridge is absent
 * or refuses the call.
 */
export async function callFigmog(tool: string, args: Record<string, unknown> = {}): Promise<CallToolResult> {
  const bridge = info();
  if (bridge === null) throw new Error("the figmog bridge is only available on the Vite dev server");
  const response = await fetch(bridge.url, {
    body: JSON.stringify({ arguments: args, tool }),
    headers: { "content-type": "application/json", "x-figmog-token": bridge.token },
    method: "POST",
  });
  const body = (await response.json()) as CallToolResult | { error: string };
  if (!response.ok || "error" in body) {
    throw new Error("error" in body ? body.error : `figmog bridge: HTTP ${response.status}`);
  }
  return body;
}

/** The parsed JSON of a read tool's text result (every figmog read is one JSON text block). */
export function resultJson(result: CallToolResult): unknown {
  const first = result.content[0];
  if (result.isError === true) throw new Error(first?.type === "text" ? first.text : "figmog tool error");
  if (first?.type !== "text") throw new Error("figmog result has no text block");
  return JSON.parse(first.text) as unknown;
}
