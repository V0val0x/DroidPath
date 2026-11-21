import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { renderSpatialGraphSvg } from "./graph-view";
import {
  AgenticHudController,
  orientDisplayDimensions,
  stageToNormalizedDevice,
} from "./hud-overlay";
import type {
  ConnectDeviceResult,
  DroidPathDevice,
  DroidPathGraphStats,
  MacroExecutionEvent,
  McpServerStatus,
  PassiveLearningEvent,
  SpatialGraphSnapshot,
  WireSecureScreenPayload,
} from "./types";
import { isWebCodecsSupported, WebCodecsH264Player } from "./webcodecs-player";

const el = <T extends HTMLElement>(id: string): T => {
  const found = document.getElementById(id);
  if (!found) throw new Error(`Missing element #${id}`);
  return found as T;
};

const deviceSelect = el<HTMLSelectElement>("device-select");
const btnRefresh = el<HTMLButtonElement>("btn-refresh");
const btnConnect = el<HTMLButtonElement>("btn-connect");
const btnDisconnect = el<HTMLButtonElement>("btn-disconnect");
const btnPlaceholderConnect = el<HTMLButtonElement>("btn-placeholder-connect");
const btnMcpToggle = el<HTMLButtonElement>("btn-mcp-toggle");
const pillDaemon = el<HTMLElement>("pill-daemon");
const pillMirror = el<HTMLElement>("pill-mirror");
const mirrorStage = el<HTMLElement>("mirror-stage");
const mirrorCanvas = el<HTMLCanvasElement>("mirror-canvas");
const snapshotImg = el<HTMLImageElement>("snapshot-img");
const mirrorPlaceholder = el<HTMLElement>("mirror-placeholder");
const hudSvg = document.getElementById("hud-svg") as unknown as SVGSVGElement;
const hudToast = el<HTMLElement>("hud-toast");
const hudToastTitle = el<HTMLElement>("hud-toast-title");
const hudToastSub = el<HTMLElement>("hud-toast-subtitle");
const hitlCard = el<HTMLElement>("hitl-card");
const hitlCat = el<HTMLElement>("hitl-category");
const hitlReason = el<HTMLElement>("hitl-reason");
const hitlMeta = el<HTMLElement>("hitl-meta");
const btnHitlApprove = el<HTMLButtonElement>("btn-hitl-approve");
const btnHitlCancel = el<HTMLButtonElement>("btn-hitl-cancel");
const passiveToast = el<HTMLElement>("passive-toast");
const passiveText = el<HTMLElement>("passive-text");
const chkPassiveLearn = el<HTMLInputElement>("chk-passive-learn");
const fpsBadge = el<HTMLElement>("fps-badge");
const navForm = el<HTMLFormElement>("nav-form");
const navTargetInput = el<HTMLInputElement>("nav-target-input");
const navResult = el<HTMLElement>("nav-result");
const statVertices = el<HTMLElement>("stat-vertices");
const statEdges = el<HTMLElement>("stat-edges");
const statPackage = el<HTMLElement>("stat-package");
const statHash = el<HTMLElement>("stat-hash");
const graphSvg = document.getElementById("graph-svg") as unknown as SVGSVGElement;
const graphEmpty = el<HTMLElement>("graph-empty");
const btnRefreshGraph = el<HTMLButtonElement>("btn-refresh-graph");
const eventLog = el<HTMLElement>("event-log");
const btnClearLog = el<HTMLButtonElement>("btn-clear-log");

let videoWidth = 1080;
let videoHeight = 2400;
let baseDisplayWidth = 1080;
let baseDisplayHeight = 2400;
let connected = false;
let usingWebCodecs = false;
let snapshotTimer: number | null = null;
let passiveToastTimer: number | null = null;
let mcpRunning = false;
let lastGraphSignature = "";

function syncHudDisplayDimensions(): void {
  const oriented = orientDisplayDimensions(
    baseDisplayWidth,
    baseDisplayHeight,
    videoWidth,
    videoHeight
  );
  hud.setDeviceDimensions(oriented.width, oriented.height);
}

const hud = new AgenticHudController({
  svg: hudSvg,
  stage: mirrorStage,
  toastEl: hudToast,
  toastTitleEl: hudToastTitle,
  toastSubEl: hudToastSub,
  hitlCardEl: hitlCard,
  hitlCatEl: hitlCat,
  hitlReasonEl: hitlReason,
  hitlMetaEl: hitlMeta,
});

const player = new WebCodecsH264Player(mirrorCanvas, {
  onDimensions: (w, h) => {
    videoWidth = w;
    videoHeight = h;
    syncHudDisplayDimensions();
  },
  onFps: (fps) => {
    fpsBadge.textContent = `${fps} fps`;
  },
  onStatus: (status) => {
    pillMirror.textContent = `Mirror: ${status}`;
    pillMirror.className = status.includes("Connected") ? "pill pill-ok" : "pill";
  },
  onError: (err) => {
    appendLog(`VideoDecoder warning: ${err}`);
    startSnapshotFallback();
  },
});

function appendLog(message: string): void {
  const row = document.createElement("div");
  row.className = "log-entry";
  const ts = new Date().toLocaleTimeString([], { hour12: false });
  row.innerHTML = `<span class="log-time">${ts}</span>`;
  row.appendChild(document.createTextNode(message));
  eventLog.prepend(row);
  while (eventLog.childElementCount > 100) {
    eventLog.lastElementChild?.remove();
  }
}

function stopSnapshotFallback(): void {
  if (snapshotTimer !== null) {
    window.clearInterval(snapshotTimer);
    snapshotTimer = null;
  }
}

function startSnapshotFallback(): void {
  if (!connected || snapshotTimer !== null) return;
  usingWebCodecs = false;
  mirrorCanvas.classList.add("hidden");
  snapshotImg.classList.remove("hidden");
  pillMirror.textContent = "Mirror: Screencap Fallback";
  pillMirror.className = "pill pill-warn";

  snapshotImg.onload = () => {
    const nw = snapshotImg.naturalWidth;
    const nh = snapshotImg.naturalHeight;
    if (nw > 0 && nh > 0 && (videoWidth !== nw || videoHeight !== nh)) {
      videoWidth = nw;
      videoHeight = nh;
      syncHudDisplayDimensions();
    }
  };

  const pull = async () => {
    if (!connected) return;
    try {
      const b64 = await invoke<string>("capture_screenshot_png");
      snapshotImg.src = `data:image/png;base64,${b64}`;
      fpsBadge.textContent = "snap";
    } catch {
      // Ignore transient capture errors
    }
  };
  void pull();
  snapshotTimer = window.setInterval(() => void pull(), 400);
}

async function refreshDevices(): Promise<void> {
  try {
    const devices = await invoke<DroidPathDevice[]>("list_devices");
    deviceSelect.innerHTML = "";
    if (devices.length === 0) {
      const opt = document.createElement("option");
      opt.value = "";
      opt.textContent = "No ADB devices detected";
      deviceSelect.appendChild(opt);
      return;
    }
    for (const dev of devices) {
      const opt = document.createElement("option");
      opt.value = dev.serial;
      opt.textContent = `${dev.model} (${dev.serial}) [${dev.is_wireless ? "Wi-Fi" : "USB"}]`;
      deviceSelect.appendChild(opt);
    }
  } catch (err) {
    appendLog(`ADB scan error: ${String(err)}`);
  }
}

async function refreshGraph(): Promise<void> {
  try {
    const snap = await invoke<SpatialGraphSnapshot>("get_graph_snapshot");
    renderSpatialGraphSvg(graphSvg, graphEmpty, snap, (vertex) => {
      navTargetInput.value = vertex.sampleInteractiveLabels[0] ?? vertex.layoutHashHex;
      void runNavigation(undefined, vertex.layoutHashHex, false);
    });
  } catch (err) {
    appendLog(`Graph refresh error: ${String(err)}`);
  }
}

function updateStatsUI(stats: DroidPathGraphStats): void {
  const seedCount = stats.seedVertexCount ?? 0;
  const hydratedCount = stats.hydratedVertexCount ?? Math.max(0, stats.vertexCount - seedCount);
  statVertices.textContent =
    seedCount > 0
      ? `${stats.vertexCount} (${hydratedCount} real · ${seedCount} seed)`
      : String(stats.vertexCount);
  statEdges.textContent = String(stats.edgeCount);
  statPackage.textContent = stats.currentPackage || "—";
  statHash.textContent = stats.currentStateHashHex ?? "—";

  const isLive = stats.hasFreshShadowDom || stats.connectionState.status === "connected";
  pillDaemon.textContent = isLive
    ? "Daemon: Connected (:27189)"
    : `Daemon: ${stats.connectionState.status}`;
  pillDaemon.className = isLive ? "pill pill-ok" : "pill";

  const graphSig = `${stats.vertexCount}:${hydratedCount}:${seedCount}:${stats.edgeCount}:${stats.currentStateHashHex ?? ""}`;
  if (graphSig !== lastGraphSignature) {
    lastGraphSignature = graphSig;
    void refreshGraph();
  }
}

async function connectSelected(): Promise<void> {
  const serial = deviceSelect.value || undefined;
  btnConnect.disabled = true;
  appendLog(`Connecting to ${serial ?? "first available device"}...`);
  try {
    const res = await invoke<ConnectDeviceResult>("connect_device", {
      serial,
      deployDaemon: true,
      startMirror: true,
    });
    connected = true;
    videoWidth = res.videoWidth;
    videoHeight = res.videoHeight;
    baseDisplayWidth = res.displayWidth || res.videoWidth;
    baseDisplayHeight = res.displayHeight || res.videoHeight;
    syncHudDisplayDimensions();
    mirrorPlaceholder.classList.add("hidden");
    btnConnect.classList.add("hidden");
    btnDisconnect.classList.remove("hidden");

    if (res.daemonWarning) {
      appendLog(`Daemon warning: ${res.daemonWarning}`);
    } else if (res.daemonReady) {
      appendLog(`Shadow DOM daemon ready on ${res.serial}`);
    }

    if (res.mirror?.wsUrl && isWebCodecsSupported()) {
      stopSnapshotFallback();
      usingWebCodecs = true;
      snapshotImg.classList.add("hidden");
      mirrorCanvas.classList.remove("hidden");
      player.start(res.mirror.wsUrl);
      appendLog(
        `Streaming H.264 via WebCodecs (${res.videoWidth}x${res.videoHeight}, scrcpy ${res.mirror.scrcpyVersion})`
      );
    } else {
      if (res.mirrorWarning) appendLog(`Mirror fallback: ${res.mirrorWarning}`);
      startSnapshotFallback();
    }

    await refreshGraph();
  } catch (err) {
    appendLog(`Connect failed: ${String(err)}`);
  } finally {
    btnConnect.disabled = false;
  }
}

async function disconnectCurrent(): Promise<void> {
  connected = false;
  usingWebCodecs = false;
  player.stop();
  stopSnapshotFallback();
  hud.clear();
  try {
    await invoke("disconnect_device");
  } catch {
    // Ignore
  }
  mirrorPlaceholder.classList.remove("hidden");
  btnConnect.classList.remove("hidden");
  btnDisconnect.classList.add("hidden");
  pillMirror.textContent = "Mirror: Idle";
  pillMirror.className = "pill";
  fpsBadge.textContent = "-- fps";
  appendLog("Disconnected.");
}

async function runNavigation(
  target?: string,
  targetHash?: string,
  confirmed = false
): Promise<void> {
  navResult.classList.remove("hidden");
  navResult.textContent = `Routing via A* to ${target ?? targetHash}...`;
  try {
    const res = await invoke<Record<string, unknown>>("navigate_to_node", {
      target,
      targetHash,
      confirmed,
      autoExplore: true,
    });
    navResult.textContent = JSON.stringify(res, null, 2);
    appendLog(`A* route result: ${String(res.status ?? "done")}`);
    await refreshGraph();
  } catch (err) {
    navResult.textContent = `Error: ${String(err)}`;
    appendLog(`Route error: ${String(err)}`);
  }
}

// Pointer / Touch / Swipe handling with Passive Learning attribution
let pointerDown = false;
let downNorm = { x: 0, y: 0 };
let downTime = 0;

mirrorStage.addEventListener("pointerdown", (e) => {
  if (!connected || e.button !== 0) return;
  if ((e.target as HTMLElement).closest(".hitl-card")) return;
  const mapped = stageToNormalizedDevice(
    e.clientX,
    e.clientY,
    mirrorStage.getBoundingClientRect(),
    videoWidth,
    videoHeight
  );
  if (!mapped.inside) return;

  pointerDown = true;
  downNorm = { x: mapped.normX, y: mapped.normY };
  downTime = performance.now();
  mirrorStage.setPointerCapture(e.pointerId);

  if (usingWebCodecs) {
    void invoke("inject_touch", {
      action: 0,
      normX: mapped.normX,
      normY: mapped.normY,
      recordPassiveTap: false,
    });
  }
});

mirrorStage.addEventListener("pointermove", (e) => {
  if (!connected || !pointerDown || !usingWebCodecs) return;
  const mapped = stageToNormalizedDevice(
    e.clientX,
    e.clientY,
    mirrorStage.getBoundingClientRect(),
    videoWidth,
    videoHeight
  );
  void invoke("inject_touch", {
    action: 2,
    normX: mapped.normX,
    normY: mapped.normY,
    recordPassiveTap: false,
  });
});

mirrorStage.addEventListener("pointercancel", (e) => {
  if (!pointerDown) return;
  pointerDown = false;
  if (connected && usingWebCodecs) {
    const mapped = stageToNormalizedDevice(
      e.clientX,
      e.clientY,
      mirrorStage.getBoundingClientRect(),
      videoWidth,
      videoHeight
    );
    void invoke("inject_touch", {
      action: 3, // AMOTION_EVENT_ACTION_CANCEL
      normX: mapped.normX,
      normY: mapped.normY,
      recordPassiveTap: false,
    });
  }
});

mirrorStage.addEventListener("pointerup", (e) => {
  if (!connected || !pointerDown) return;
  pointerDown = false;
  const mapped = stageToNormalizedDevice(
    e.clientX,
    e.clientY,
    mirrorStage.getBoundingClientRect(),
    videoWidth,
    videoHeight
  );
  const oriented = orientDisplayDimensions(
    baseDisplayWidth,
    baseDisplayHeight,
    videoWidth,
    videoHeight
  );
  const dx = (mapped.normX - downNorm.x) * oriented.width;
  const dy = (mapped.normY - downNorm.y) * oriented.height;
  const dist = Math.hypot(dx, dy);
  const elapsedMs = Math.max(80, Math.round(performance.now() - downTime));
  const passiveLearn = chkPassiveLearn.checked;

  if (dist > 36) {
    // Record passive swipe BEFORE releasing touch so pendingAction beats the post-fling DOM transition
    if (passiveLearn || !usingWebCodecs) {
      void invoke("inject_swipe", {
        startNormX: downNorm.x,
        startNormY: downNorm.y,
        endNormX: mapped.normX,
        endNormY: mapped.normY,
        durationMs: elapsedMs,
        dispatchViaAdb: !usingWebCodecs,
      }).finally(() => {
        if (usingWebCodecs) {
          void invoke("inject_touch", {
            action: 1,
            normX: mapped.normX,
            normY: mapped.normY,
            recordPassiveTap: false,
          });
        }
      });
    } else if (usingWebCodecs) {
      void invoke("inject_touch", {
        action: 1,
        normX: mapped.normX,
        normY: mapped.normY,
        recordPassiveTap: false,
      });
    }
  } else {
    void invoke("inject_touch", {
      action: 1,
      normX: mapped.normX,
      normY: mapped.normY,
      recordPassiveTap: passiveLearn,
    });
  }
});

mirrorStage.addEventListener(
  "wheel",
  (e) => {
    if (!connected || !usingWebCodecs) return;
    e.preventDefault();
    const mapped = stageToNormalizedDevice(
      e.clientX,
      e.clientY,
      mirrorStage.getBoundingClientRect(),
      videoWidth,
      videoHeight
    );
    const dh = Math.max(-1, Math.min(1, -e.deltaX / 120));
    const dv = Math.max(-1, Math.min(1, -e.deltaY / 120));
    void invoke("inject_scroll", {
      normX: mapped.normX,
      normY: mapped.normY,
      deltaH: dh,
      deltaV: dv,
    });
  },
  { passive: false }
);

mirrorStage.addEventListener("keydown", (e) => {
  if (!connected) return;
  if (e.key === "Escape") {
    void invoke("inject_keyevent", { keycode: "BACK" });
  } else if (e.key === "Enter") {
    void invoke("inject_keyevent", { keycode: "ENTER" });
  } else if (e.key === "Backspace") {
    void invoke("inject_keyevent", { keycode: "DELETE" });
  } else if (e.key.length === 1 && !e.metaKey && !e.ctrlKey) {
    void invoke("inject_text", { text: e.key, pressEnter: false });
  }
});

// UI Button Bindings
btnRefresh.addEventListener("click", () => void refreshDevices());
btnConnect.addEventListener("click", () => void connectSelected());
btnPlaceholderConnect.addEventListener("click", () => void connectSelected());
btnDisconnect.addEventListener("click", () => void disconnectCurrent());
btnRefreshGraph.addEventListener("click", () => void refreshGraph());
btnClearLog.addEventListener("click", () => {
  eventLog.innerHTML = "";
});

navForm.addEventListener("submit", (e) => {
  e.preventDefault();
  const q = navTargetInput.value.trim();
  if (!q) return;
  if (q.startsWith("0x")) {
    void runNavigation(undefined, q, false);
  } else {
    void runNavigation(q, undefined, false);
  }
});

btnHitlApprove.addEventListener("click", async () => {
  hud.hideHitl();
  appendLog("HITL approved by user — resuming hazardous action...");
  const res = await invoke<Record<string, unknown>>("resolve_hitl_confirmation", {
    approved: true,
  });
  navResult.classList.remove("hidden");
  navResult.textContent = JSON.stringify(res, null, 2);
  await refreshGraph();
});

btnHitlCancel.addEventListener("click", async () => {
  hud.hideHitl();
  appendLog("HITL cancelled by user.");
  await invoke("resolve_hitl_confirmation", { approved: false });
});

btnMcpToggle.addEventListener("click", async () => {
  try {
    const status = await invoke<McpServerStatus>("toggle_mcp_http", {
      enable: !mcpRunning,
      port: 8766,
    });
    mcpRunning = status.running;
    btnMcpToggle.textContent = status.running ? `MCP: :${status.port}` : "MCP: Off";
    btnMcpToggle.className = status.running
      ? "pill pill-ok pill-interactive"
      : "pill pill-interactive";
    if (status.sseUrl) {
      appendLog(`Embedded MCP HTTP/SSE listening on ${status.sseUrl}`);
    } else {
      appendLog("Embedded MCP HTTP/SSE stopped.");
    }
  } catch (err) {
    appendLog(`MCP server toggle failed: ${String(err)}`);
  }
});

document.querySelectorAll<HTMLButtonElement>(".hw-btn[data-key]").forEach((btn) => {
  btn.addEventListener("click", () => {
    const keycode = btn.dataset.key;
    if (keycode) {
      void invoke("inject_keyevent", { keycode });
    }
  });
});

window.addEventListener("resize", () => hud.renderReticle());

// Subscribe to Real-Time Rust Tauri Events
async function init(): Promise<void> {
  await listen<MacroExecutionEvent>("hud-event", (ev) => {
    hud.handleEvent(ev.payload);
    if (ev.payload.type === "completed" || ev.payload.type === "failed") {
      void refreshGraph();
    }
  });

  await listen<WireSecureScreenPayload>("secure-screen", (ev) => {
    hud.handleSecureScreen(ev.payload);
  });

  await listen<DroidPathGraphStats>("graph-stats", (ev) => {
    updateStatsUI(ev.payload);
  });

  await listen<PassiveLearningEvent>("passive-learning", (ev) => {
    const p = ev.payload;
    if (typeof p.x === "number" && typeof p.y === "number") {
      hud.flashPassiveClick(p.x, p.y);
    }
    passiveText.textContent = `+ Learned ${p.kind}: ${p.label}`;
    passiveToast.classList.remove("hidden");
    if (passiveToastTimer) window.clearTimeout(passiveToastTimer);
    passiveToastTimer = window.setTimeout(() => passiveToast.classList.add("hidden"), 2000);
    appendLog(`Passive learn [${p.kind}]: ${p.label}`);
    window.setTimeout(() => void refreshGraph(), 400);
  });

  await refreshDevices();
  try {
    const stats = await invoke<DroidPathGraphStats>("get_graph_stats");
    updateStatsUI(stats);
    await refreshGraph();
  } catch {
    // Running outside Tauri shell (e.g., vite preview)
  }
}

void init();
