// Tauri 2 exposes the API on window.__TAURI__ when withGlobalTauri is true.
// https://v2.tauri.app/reference/javascript/api/
const { invoke, Channel } = window.__TAURI__.core;

const term = new Terminal({
  fontFamily: 'Menlo, Monaco, "Courier New", monospace',
  fontSize: 13,
  cursorBlink: true,
  theme: { background: "#000000", foreground: "#e0e0e0" },
  scrollback: 10000,
  allowProposedApi: true,
});

const fitAddon = new FitAddon.FitAddon();
term.loadAddon(fitAddon);

const container = document.getElementById("terminal");
term.open(container);

try {
  const webgl = new WebglAddon.WebglAddon();
  term.loadAddon(webgl);
  webgl.onContextLoss(() => webgl.dispose());
} catch (e) {
  console.warn("webgl addon failed, falling back to canvas/dom renderer", e);
}

fitAddon.fit();
window.addEventListener("resize", () => fitAddon.fit());

// PTY wiring: stdout from Rust arrives over a Channel; stdin goes via invoke.
// https://v2.tauri.app/develop/calling-frontend/#channels
const ptyChannel = new Channel();
const transcriptDecoder = new TextDecoder();
let transcriptWriteQueue = Promise.resolve();
let transcriptAgentSectionOpen = false;
let transcriptAgentLineBuffer = "";
let transcriptAgentLastLineBlank = true;
let shellEscapeState = null;
let transcriptOutputEscapeState = null;

function queueAgentEchoAppend(text) {
  if (!text) return;
  transcriptWriteQueue = transcriptWriteQueue
    .then(() => invoke("append_agent_echo", { text }))
    .catch((e) => console.error("append_agent_echo", e));
}

function sanitizeTranscriptText(text) {
  const source = String(text ?? "");
  let result = "";

  for (const ch of source) {
    if (transcriptOutputEscapeState === "osc") {
      if (ch === "\u0007") {
        transcriptOutputEscapeState = null;
      } else if (ch === "\u001b") {
        transcriptOutputEscapeState = "st";
      }
      continue;
    }

    if (transcriptOutputEscapeState === "st") {
      if (ch === "\\") {
        transcriptOutputEscapeState = null;
      } else if (ch !== "\u001b") {
        transcriptOutputEscapeState =
          transcriptOutputEscapeState === "st-from-dcs" ? "dcs" : "osc";
      }
      continue;
    }

    if (transcriptOutputEscapeState === "csi" || transcriptOutputEscapeState === "esc") {
      if (ch >= "@" && ch <= "~") {
        transcriptOutputEscapeState = null;
        continue;
      }
      if (transcriptOutputEscapeState === "esc" && ch === "]") {
        transcriptOutputEscapeState = "osc";
        continue;
      }
      if (transcriptOutputEscapeState === "esc" && ch === "P") {
        transcriptOutputEscapeState = "dcs";
        continue;
      }
      if (transcriptOutputEscapeState === "esc" && ch === "[") {
        transcriptOutputEscapeState = "csi";
        continue;
      }
      continue;
    }

    if (transcriptOutputEscapeState === "dcs") {
      if (ch === "\u001b") {
        transcriptOutputEscapeState = "st-from-dcs";
      }
      continue;
    }

    if (ch === "\u001b") {
      transcriptOutputEscapeState = "esc";
      continue;
    }
    if (ch === "\u0007") {
      continue;
    }
    if (ch === "\r") {
      result += "\n";
      continue;
    }
    if (ch === "\n" || ch === "\t" || ch >= " ") {
      result += ch;
    }
  }

  return result.replace(/\r\n/g, "\n");
}

function stripTranscriptNoise(text) {
  return String(text ?? "")
    .replace(/0;[^\n]*?xmlui-claude-code-des\.\.\./g, "")
    .replace(/(?:^|\s)Working\(\d+s • esc to interrupt\)/g, "")
    .replace(/(?:^|\s)Starting MCP servers[^\n]*/g, "")
    .replace(/(?:^|\s)Booting MCP server:[^\n]*/g, "")
    .replace(/(?:^|\s)Tip: New Use \/fast[^\n]*/g, "")
    .replace(/› [^\n]*gpt-[^\n]*/g, "")
    .replace(/· \d+ background terminal running · \/ps to view · \/stop to close/g, "")
    .replace(/■ Conversation interrupted[^\n]*/g, "");
}

function isTranscriptNoiseLine(line) {
  const normalized = String(line ?? "").trim();
  if (!normalized) return false;
  if (/^[╭╰│─└┌┐┘├┤┬┴┼\-\s]+$/u.test(normalized)) return true;
  if (/^› /.test(normalized)) return true;
  if (normalized.includes("OpenAI Codex (v")) return true;
  if (normalized.includes("model:")) return true;
  if (normalized.includes("directory:")) return true;
  if (normalized.includes("Tip:")) return true;
  if (normalized.includes("Starting MCP servers")) return true;
  if (normalized.includes("Booting MCP server:")) return true;
  if (normalized.includes("Sort:UpdatedType to search")) return true;
  if (normalized.includes("CreatedUpdatedBranchConversation")) return true;
  if (normalized.includes("Loading sessions")) return true;
  if (normalized.includes("enter to resume")) return true;
  if (normalized.includes("choose what model and reasoning effort to use")) return true;
  if (normalized.includes("xmlui-claude-code-des...")) return true;
  if (normalized.includes("Hit `/feedback` to report the issue.")) return true;
  if (normalized.includes("Working(")) return true;
  if (/W{2,}o{2,}r{2,}k{2,}/.test(normalized)) return true;
  if (/^[•◦]\s*$/.test(normalized)) return true;
  if (/[⠁-⣿]/u.test(normalized)) return true;
  return false;
}

function filterTranscriptAgentChunk(text, flushPartial = false) {
  transcriptAgentLineBuffer += stripTranscriptNoise(text);
  const lines = transcriptAgentLineBuffer.split("\n");
  transcriptAgentLineBuffer = flushPartial ? "" : lines.pop();

  const output = [];
  for (const rawLine of lines) {
    const line = rawLine.replace(/[ \t]+$/g, "");
    if (isTranscriptNoiseLine(line)) continue;
    if (!line.trim()) {
      if (!transcriptAgentLastLineBlank) {
        output.push("");
        transcriptAgentLastLineBlank = true;
      }
      continue;
    }
    output.push(line);
    transcriptAgentLastLineBlank = false;
  }

  if (flushPartial) {
    const trailing = stripTranscriptNoise(transcriptAgentLineBuffer).replace(/[ \t]+$/g, "");
    transcriptAgentLineBuffer = "";
    if (trailing.trim() && !isTranscriptNoiseLine(trailing)) {
      output.push(trailing);
      transcriptAgentLastLineBlank = false;
    }
  }

  return output.join("\n");
}

function sanitizeTranscriptUserText(text) {
  return String(text ?? "")
    .replace(/\u001b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, " ")
    .replace(/\u001bP[\s\S]*?(?:\x1b\\)/g, " ")
    .replace(/\u001b(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])/g, " ")
    .replace(/[IO]?\??\d+(?:;\d+)*[Rc]/g, " ")
    .replace(/[IO]?(?:10|11);rgb:[0-9a-f/]+/gi, " ")
    .replace(/\?(?:10|11);rgb:[0-9a-f/]+/gi, " ")
    .replace(/[IO]\?/g, " ")
    .replace(/\?(?=\/)/g, " ")
    .replace(/Hit `\/feedback` to report the issue\./g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

function appendTranscriptUserTurn(text) {
  const trimmed = sanitizeTranscriptUserText(text);
  if (!trimmed) return;
  const pendingAgentText = filterTranscriptAgentChunk("", true);
  if (pendingAgentText) {
    queueAgentEchoAppend(
      transcriptAgentSectionOpen ? pendingAgentText : `\n\n### Agent\n\n${pendingAgentText}`
    );
    transcriptAgentSectionOpen = true;
  }
  transcriptAgentSectionOpen = false;
  queueAgentEchoAppend(`\n\n### User\n\n${trimmed}`);
}

function appendTranscriptAgentOutput(text) {
  const filtered = filterTranscriptAgentChunk(sanitizeTranscriptText(text));
  if (!filtered) return;
  if (!transcriptAgentSectionOpen) {
    transcriptAgentSectionOpen = true;
    queueAgentEchoAppend(`\n\n### Agent\n\n${filtered}`);
    return;
  }
  queueAgentEchoAppend(filtered);
}

ptyChannel.onmessage = (chunk) => {
  // chunk is a number[] (bytes) coming from Rust
  term.write(new Uint8Array(chunk));
  appendTranscriptAgentOutput(transcriptDecoder.decode(new Uint8Array(chunk), { stream: true }));
};

let shellLineBuffer = "";

function consumeShellInputChar(ch) {
  if (shellEscapeState === "osc") {
    if (ch === "\u0007") {
      shellEscapeState = null;
    }
    return;
  }

  if (shellEscapeState === "st") {
    if (ch === "\\") {
      shellEscapeState = null;
    } else if (ch !== "\u001b") {
      shellEscapeState = "osc";
    }
    return;
  }

  if (shellEscapeState === "csi" || shellEscapeState === "esc") {
    if (ch >= "@" && ch <= "~") {
      shellEscapeState = null;
      return;
    }
    if (shellEscapeState === "esc" && ch === "]") {
      shellEscapeState = "osc";
      return;
    }
    if (shellEscapeState === "esc" && ch === "P") {
      shellEscapeState = "dcs";
      return;
    }
    if (shellEscapeState === "esc" && ch === "[") {
      shellEscapeState = "csi";
      return;
    }
    return;
  }

  if (shellEscapeState === "dcs") {
    if (ch === "\u001b") {
      shellEscapeState = "st";
    }
    return;
  }

  if (ch === "\u001b") {
    shellEscapeState = "esc";
    return;
  }

  if (ch >= " " && ch !== "\u007f") {
    shellLineBuffer += ch;
  }
}

function maybeTriggerSessionRollover(commandLine) {
  const command = String(commandLine ?? "").trim();
  if (/^(claude|codex)(\s|$)/.test(command)) {
    ensureAgentEchoSessionRollover();
  }
}

term.onData((data) => {
  for (const ch of data) {
    if (ch === "\r") {
      maybeTriggerSessionRollover(shellLineBuffer);
      appendTranscriptUserTurn(shellLineBuffer);
      shellLineBuffer = "";
      continue;
    }
    if (ch === "\u0003") {
      shellLineBuffer = "";
      continue;
    }
    if (ch === "\u0015") {
      shellLineBuffer = "";
      continue;
    }
    if (ch === "\u007f") {
      shellLineBuffer = shellLineBuffer.slice(0, -1);
      continue;
    }
    consumeShellInputChar(ch);
  }
  invoke("pty_write", { data }).catch((e) => console.error("pty_write", e));
});

term.onResize(({ cols, rows }) => {
  invoke("pty_resize", { cols, rows }).catch((e) => console.error("pty_resize", e));
});

(async () => {
  try {
    await invoke("pty_spawn", {
      cmd: "/bin/bash",
      args: ["--noprofile", "--rcfile", "./app/shell/claude-code-shellrc", "-i"],
      cols: term.cols,
      rows: term.rows,
      onData: ptyChannel,
    });
    term.focus();
  } catch (e) {
    term.writeln(`\r\n\x1b[31mfailed to start pty: ${e}\x1b[0m`);
  }
})();

// Right pane → parent shell dispatcher. The iframe posts events declaring
// one of three intents:
//   to-shell      — inject text into the PTY
//   log           — record in cargo run stderr only
//   open-devtools — internal command
//   open-url      — open an external URL with the host OS
window.addEventListener("message", (ev) => {
  if (!ev.data || ev.data.type !== "right-pane") return;
  const data = ev.data;

  switch (data.kind) {
    case "to-shell":
      appendTranscriptUserTurn(String(data.text ?? ""));
      invoke("pty_write", { data: (data.text ?? "") + "\n" }).catch((e) =>
        console.error("pty_write inject", e),
      );
      return;
    case "to-turn":
      appendTranscriptUserTurn(String(data.text ?? ""));
      invoke("pty_write", {
        data: "\x1b[200~" + String(data.text ?? "") + "\x1b[201~\r",
      }).catch((e) => console.error("pty_write turn", e));
      return;
    case "open-devtools":
      invoke("open_devtools").catch((e) =>
        console.error("open_devtools", e),
      );
      return;
    case "open-url":
      invoke("open_url", { url: String(data.url ?? "") }).catch((e) =>
        console.error("open_url", e),
      );
      return;
    case "save-trace-export":
      invoke("save_trace_export", {
        filename: String(data.filename ?? "xs-trace.json"),
        content: String(data.content ?? ""),
        mimeType: String(data.mimeType ?? "application/octet-stream"),
      })
        .then((path) => {
          if (ev.source && typeof ev.source.postMessage === "function") {
            ev.source.postMessage(
              {
                type: "save-trace-export-result",
                requestId: data.requestId,
                ok: true,
                path,
              },
              "*",
            );
          }
        })
        .catch((e) => {
          if (ev.source && typeof ev.source.postMessage === "function") {
            ev.source.postMessage(
              {
                type: "save-trace-export-result",
                requestId: data.requestId,
                ok: false,
                error: String(e?.message ?? e ?? "export failed"),
              },
              "*",
            );
          }
          console.error("save_trace_export", e);
        });
      return;
    case "log":
    default:
      invoke("log_from_right_pane", {
        payload: data.payload ?? data,
      }).catch((e) => console.error("log_from_right_pane", e));
      return;
  }
});

// Reassigning src works cross-origin; iframe.contentWindow.location.reload()
// is blocked because the parent shell is on tauri:// and the iframe on xmlui://.
const RIGHT_PANE_SRC = "xmlui://localhost/right/index.html";
function reloadRightPane() {
  const iframe = document.getElementById("right-pane");
  if (!iframe) return;
  iframe.src = RIGHT_PANE_SRC + "?t=" + Date.now();
}

function pad2(value) {
  return String(value).padStart(2, "0");
}

function getSessionTimestampParts(now = new Date()) {
  const year = now.getFullYear();
  const month = pad2(now.getMonth() + 1);
  const day = pad2(now.getDate());
  const hours = pad2(now.getHours());
  const minutes = pad2(now.getMinutes());
  const seconds = pad2(now.getSeconds());

  return {
    archiveTimestamp: `${year}-${month}-${day}-${hours}-${minutes}-${seconds}`,
    createdAt: `${year}-${month}-${day} ${hours}:${minutes}:${seconds}`,
  };
}

async function ensureAgentEchoSessionRollover() {
  const { archiveTimestamp, createdAt } = getSessionTimestampParts();
  try {
    const rolledOver = await invoke("rollover_agent_echo_session", {
      archiveTimestamp,
      createdAt,
    });
    if (rolledOver) {
      reloadRightPane();
    }
  } catch (e) {
    console.error("rollover_agent_echo_session", e);
  }
}

// Manual reload button in the toolbar.
document
  .getElementById("reload-right")
  ?.addEventListener("click", reloadRightPane);

// Live reload: Rust filesystem watcher emits "right-pane-reload" when files in
// app/right/ change. https://v2.tauri.app/develop/calling-frontend/#event-system
const { listen } = window.__TAURI__.event;
listen("right-pane-reload", reloadRightPane);
ensureAgentEchoSessionRollover();
