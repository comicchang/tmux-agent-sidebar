/**
 * tmux-agent-sidebar bridge for Oh My Pi.
 *
 * Subscribes to OMP agent lifecycle events and forwards them to the
 * sidebar's hook.sh script so the TUI can display agent status in
 * real time.
 *
 * Subagent handling: OMP subagents share the same tmux pane as the
 * main agent.  Only the main agent fires session-start/session-end;
 * subagents fire subagent-start/subagent-stop via the task tool.
 * We detect subagents by checking whether @pane_agent is already set.
 */
import type { HookAPI, HookFactory } from "@oh-my-pi/pi-coding-agent";

// ─── Configuration ──────────────────────────────────────────────────

const HOOK_SCRIPT = (() => {
  const raw = "~/.byobu/plugins/tmux-agent-sidebar/hook.sh";
  if (raw.startsWith("~/")) {
    const home = process.env.HOME ?? "/Users/unknown";
    return home + raw.slice(1);
  }
  return raw;
})();

const AGENT = "pi";

// ─── Helpers ────────────────────────────────────────────────────────

function fireHook(
  pi: HookAPI,
  event: string,
  payload: Record<string, unknown>,
): void {
  const json = JSON.stringify(payload);
  const safeJson = json.replace(/'/g, "'\\''");
  const safeScript = HOOK_SCRIPT.replace(/'/g, "'\\''");
  const cmd = `echo '${safeJson}' | bash '${safeScript}' ${AGENT} ${event}`;
  pi.exec("bash", ["-c", cmd]).catch(() => {});
}

function lastMessageText(
  messages: unknown[] | undefined,
  maxLen: number,
): string {
  if (!messages || messages.length === 0) return "";
  const last = messages[messages.length - 1] as
    | { content?: Array<{ text?: string }> }
    | undefined;
  return (
    last?.content?.map((b) => b.text ?? "").join("\n") ?? ""
  ).slice(0, maxLen);
}

/**
 * Check whether @pane_agent is already set on the current tmux pane.
 * If it is, the main agent has already initialised and we are a subagent.
 */
async function isMainAgent(pi: HookAPI): Promise<boolean> {
  const pane = process.env.TMUX_PANE;
  if (!pane) return true; // outside tmux: assume main
  try {
    const result = await pi.exec("tmux", [
      "show-option",
      "-p",
      "-t",
      pane,
      "@pane_agent",
    ]);
    return result.stdout.trim() === "";
  } catch {
    return true; // tmux unavailable: assume main
  }
}

// ─── Hook factory ───────────────────────────────────────────────────

const factory: HookFactory = (pi: HookAPI) => {
  // Session start: only the main agent fires session-start.
  // Subagents skip this entirely — they are tracked via subagent-start.
  pi.on("session_start", async (_event, ctx) => {
    if (!(await isMainAgent(pi))) return;
    fireHook(pi, "session-start", {
      cwd: ctx.cwd,
      sessionId: null,
      source: "",
    });
  });

  // Session stop: only the main agent fires session-end.
  pi.on("session_stop", async (event) => {
    if (!(await isMainAgent(pi))) return;
    fireHook(pi, "session-end", {
      endReason: lastMessageText(event.messages as unknown[], 200)
        ? "stop"
        : "clear",
      sessionId: event.session_id || null,
    });
  });

  // before_agent_start / agent_end: only main agent.
  pi.on("before_agent_start", async (event, ctx) => {
    if (!(await isMainAgent(pi))) return;
    fireHook(pi, "user-prompt-submit", {
      cwd: ctx.cwd,
      prompt: event.prompt,
      sessionId: null,
    });
  });

  pi.on("agent_end", async (event, ctx) => {
    if (!(await isMainAgent(pi))) return;
    fireHook(pi, "stop", {
      cwd: ctx.cwd,
      lastMessage: lastMessageText(event.messages as unknown[], 300),
      sessionId: null,
    });
  });

  // Tool events: activity-log for all tools; subagent start/stop for task.
  pi.on("tool_call", (event) => {
    if (event.toolName === "task") {
      const input = event.toolInput as Record<string, unknown> | undefined;
      fireHook(pi, "subagent-start", {
        agent:
          (input?.subagent_type as string) ??
          (input?.agent as string) ??
          "task",
        id: (input?.id as string) ?? null,
      });
    }
  });

  pi.on("tool_result", (event) => {
    fireHook(pi, "activity-log", {
      toolName: event.toolName,
      toolArgs: event.toolInput,
      result: event.toolOutput ?? null,
    });

    if (event.toolName === "task") {
      const input = event.toolInput as Record<string, unknown> | undefined;
      const output = event.toolOutput as Record<string, unknown> | undefined;
      fireHook(pi, "subagent-stop", {
        agent:
          (input?.subagent_type as string) ??
          (input?.agent as string) ??
          "task",
        id: (input?.id as string) ?? null,
        status: (output?.status as string) ?? "completed",
        error: (output?.error as string) ?? null,
        sessionFile: (output?.sessionFile as string) ?? null,
      });
    }
  });
};

export default factory;
