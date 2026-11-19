import type {
  AttributedUIAction,
  HazardousConfirmationRequest,
  MacroExecutionEvent,
  WireSecureScreenPayload,
} from "./types";

export type HudTelemetryMode = "autonomous" | "awaitingUserInput" | "awaitingBiometrics";
export type HudToastPhase =
  | "idle"
  | "executing"
  | "awaitingConfirmation"
  | "awaitingBiometrics"
  | "completed"
  | "failed";

export interface VideoRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface AgenticReticle {
  x: number;
  y: number;
  startX?: number | null;
  startY?: number | null;
  kind: AttributedUIAction["kind"];
  label: string;
  mode: HudTelemetryMode;
  centered?: boolean;
}

/**
 * Aligns physical Android display dimensions (`wm size`) to match the current video orientation
 * so Shadow DOM / A* HUD coordinates map accurately even when scrcpy downscales video via `max_size`.
 */
export function orientDisplayDimensions(
  displayWidth: number,
  displayHeight: number,
  videoWidth: number,
  videoHeight: number
): { width: number; height: number } {
  if (displayWidth <= 0 || displayHeight <= 0) {
    return { width: videoWidth, height: videoHeight };
  }
  if (videoWidth <= 0 || videoHeight <= 0) {
    return { width: displayWidth, height: displayHeight };
  }
  const displayLandscape = displayWidth > displayHeight;
  const videoLandscape = videoWidth > videoHeight;
  if (displayLandscape !== videoLandscape) {
    return { width: displayHeight, height: displayWidth };
  }
  return { width: displayWidth, height: displayHeight };
}

/** Computes exact aspect-fitted video rect within stage bounds (matches Swift AgenticHUDCoordinateMapper). */
export function computeVideoRect(
  stageWidth: number,
  stageHeight: number,
  deviceWidth: number,
  deviceHeight: number
): VideoRect {
  if (deviceWidth <= 0 || deviceHeight <= 0 || stageWidth <= 0 || stageHeight <= 0) {
    return { x: 0, y: 0, width: stageWidth, height: stageHeight };
  }
  const viewAspect = stageWidth / stageHeight;
  const videoAspect = deviceWidth / deviceHeight;
  if (viewAspect > videoAspect) {
    const w = stageHeight * videoAspect;
    return { x: (stageWidth - w) / 2, y: 0, width: w, height: stageHeight };
  } else {
    const h = stageWidth / videoAspect;
    return { x: 0, y: (stageHeight - h) / 2, width: stageWidth, height: h };
  }
}

export function deviceToStagePoint(
  deviceX: number,
  deviceY: number,
  stageWidth: number,
  stageHeight: number,
  deviceWidth: number,
  deviceHeight: number
): { x: number; y: number } {
  const rect = computeVideoRect(stageWidth, stageHeight, deviceWidth, deviceHeight);
  const nx = Math.max(0, Math.min(1, deviceX / Math.max(1, deviceWidth)));
  const ny = Math.max(0, Math.min(1, deviceY / Math.max(1, deviceHeight)));
  return {
    x: rect.x + nx * rect.width,
    y: rect.y + ny * rect.height,
  };
}

export function stageToNormalizedDevice(
  clientX: number,
  clientY: number,
  stageRect: DOMRect,
  deviceWidth: number,
  deviceHeight: number
): { normX: number; normY: number; inside: boolean } {
  const vRect = computeVideoRect(stageRect.width, stageRect.height, deviceWidth, deviceHeight);
  const relX = clientX - stageRect.left - vRect.x;
  const relY = clientY - stageRect.top - vRect.y;
  const inside = relX >= 0 && relX <= vRect.width && relY >= 0 && relY <= vRect.height;
  return {
    normX: Math.max(0, Math.min(1, relX / Math.max(1, vRect.width))),
    normY: Math.max(0, Math.min(1, relY / Math.max(1, vRect.height))),
    inside,
  };
}

const SVG_NS = "http://www.w3.org/2000/svg";

export class AgenticHudController {
  private svg: SVGSVGElement;
  private stage: HTMLElement;
  private toastEl: HTMLElement;
  private toastTitleEl: HTMLElement;
  private toastSubEl: HTMLElement;
  private hitlCardEl: HTMLElement;
  private hitlCatEl: HTMLElement;
  private hitlReasonEl: HTMLElement;
  private hitlMetaEl: HTMLElement;

  private deviceWidth = 1080;
  private deviceHeight = 2400;
  private activeReticle: AgenticReticle | null = null;
  private toastPhase: HudToastPhase = "idle";
  private reticleTimer: number | null = null;
  private toastTimer: number | null = null;

  constructor(elements: {
    svg: SVGSVGElement;
    stage: HTMLElement;
    toastEl: HTMLElement;
    toastTitleEl: HTMLElement;
    toastSubEl: HTMLElement;
    hitlCardEl: HTMLElement;
    hitlCatEl: HTMLElement;
    hitlReasonEl: HTMLElement;
    hitlMetaEl: HTMLElement;
  }) {
    this.svg = elements.svg;
    this.stage = elements.stage;
    this.toastEl = elements.toastEl;
    this.toastTitleEl = elements.toastTitleEl;
    this.toastSubEl = elements.toastSubEl;
    this.hitlCardEl = elements.hitlCardEl;
    this.hitlCatEl = elements.hitlCatEl;
    this.hitlReasonEl = elements.hitlReasonEl;
    this.hitlMetaEl = elements.hitlMetaEl;
  }

  public setDeviceDimensions(width: number, height: number): void {
    if (width > 0 && height > 0) {
      this.deviceWidth = width;
      this.deviceHeight = height;
      this.renderReticle();
    }
  }

  public handleEvent(event: MacroExecutionEvent): void {
    switch (event.type) {
      case "pathComputed": {
        this.hideHitl();
        const unit = event.totalSteps === 1 ? "step" : "steps";
        this.showToast(
          "executing",
          `🤖 A* Path Computed: ${event.totalSteps} ${unit}. Executing...`,
          event.targetHint ? `Target: ${event.targetHint}` : undefined
        );
        break;
      }
      case "executionStarted": {
        this.hideHitl();
        const unit = event.totalSteps === 1 ? "step" : "steps";
        if (this.toastPhase !== "executing") {
          this.showToast("executing", `🤖 A* Path Computed: ${event.totalSteps} ${unit}. Executing...`);
        }
        break;
      }
      case "willExecuteAction": {
        this.hideHitl();
        const verb = event.action.kind === "swipe" ? "Scrolling" : "A* Routing";
        const label = event.action.targetDescription || `${event.action.kind}`;
        this.showToast(
          "executing",
          `🤖 Step ${event.stepIndex + 1}/${event.totalSteps} · ${verb}`,
          label
        );
        const coords = this.extractCoords(event.action);
        if (coords) {
          this.setReticle(
            {
              x: coords.x,
              y: coords.y,
              startX: event.action.x,
              startY: event.action.y,
              kind: event.action.kind,
              label,
              mode: "autonomous",
            },
            event.action.kind === "swipe" ? 500 : 340
          );
        }
        break;
      }
      case "stepSettled": {
        if (this.toastPhase === "executing") {
          this.scheduleReticleClear(100);
        }
        break;
      }
      case "awaitingConfirmation": {
        this.showToast(
          "awaitingConfirmation",
          `⚠️ Confirm Action (${event.stepIndex + 1}/${event.totalSteps})`,
          event.reason
        );
        const coords = this.extractCoords(event.action);
        this.setReticle(
          coords
            ? {
                x: coords.x,
                y: coords.y,
                kind: "tap",
                label: event.action.targetDescription,
                mode: "awaitingUserInput",
              }
            : {
                x: 0,
                y: 0,
                kind: "tap",
                label: event.action.targetDescription,
                mode: "awaitingUserInput",
                centered: true,
              },
          null
        );
        this.showHitl(event.request);
        break;
      }
      case "awaitingBiometrics": {
        this.hideHitl();
        this.showToast(
          "awaitingBiometrics",
          "🔒 Biometrics / PIN Required",
          event.activePackage ? `${event.activePackage} · Unlock on phone` : event.reason
        );
        this.setReticle(
          {
            x: 0,
            y: 0,
            kind: "tap",
            label: event.reason,
            mode: "awaitingBiometrics",
            centered: true,
          },
          null
        );
        break;
      }
      case "biometricsCleared": {
        if (this.toastPhase === "awaitingBiometrics") {
          this.clear();
        }
        break;
      }
      case "recoveryAction": {
        if (this.toastPhase === "executing") {
          this.toastSubEl.textContent = `Auto-recovery: ${event.detail}`;
        }
        break;
      }
      case "completed": {
        this.hideHitl();
        const count = event.report.steps.length;
        const unit = count === 1 ? "step" : "steps";
        this.showToast(
          "completed",
          `🤖 Arrived in ${event.report.totalElapsedMs}ms (${count} ${unit})`,
          event.report.recoveredSteps.length > 0
            ? `Recovered: ${event.report.recoveredSteps.length}`
            : undefined,
          2800
        );
        this.scheduleReticleClear(180);
        break;
      }
      case "failed": {
        this.hideHitl();
        this.showToast("failed", "🤖 Path Execution Stopped", event.message, 3500);
        this.scheduleReticleClear(250);
        break;
      }
      case "awaitingChatInput": {
        this.showToast("awaitingConfirmation", "💬 Waiting for Input", event.prompt);
        break;
      }
    }
  }

  public handleSecureScreen(payload: WireSecureScreenPayload): void {
    if (payload.active) {
      this.handleEvent({
        type: "awaitingBiometrics",
        reason: payload.reason,
        activePackage: payload.package,
      });
    } else {
      this.handleEvent({ type: "biometricsCleared" });
    }
  }

  /** Renders a subtle emerald pulse when passive learning attributes a manual user click. */
  public flashPassiveClick(deviceX: number, deviceY: number): void {
    const rect = this.stage.getBoundingClientRect();
    const pt = deviceToStagePoint(
      deviceX,
      deviceY,
      rect.width,
      rect.height,
      this.deviceWidth,
      this.deviceHeight
    );
    const circle = document.createElementNS(SVG_NS, "circle");
    circle.setAttribute("cx", pt.x.toFixed(1));
    circle.setAttribute("cy", pt.y.toFixed(1));
    circle.setAttribute("r", "12");
    circle.setAttribute("class", "passive-click-ring");
    this.svg.appendChild(circle);
    window.setTimeout(() => circle.remove(), 480);
  }

  public clear(): void {
    if (this.reticleTimer) window.clearTimeout(this.reticleTimer);
    if (this.toastTimer) window.clearTimeout(this.toastTimer);
    this.activeReticle = null;
    this.toastPhase = "idle";
    this.toastEl.classList.add("hidden");
    this.hideHitl();
    this.renderReticle();
  }

  public hideHitl(): void {
    this.hitlCardEl.classList.add("hidden");
  }

  private showHitl(req: HazardousConfirmationRequest): void {
    this.hitlCatEl.textContent = req.hazardCategory.toUpperCase();
    this.hitlReasonEl.textContent = req.hazardReason;
    this.hitlMetaEl.textContent = `Step ${req.stepIndex + 1}/${req.totalSteps} · ${req.actionSignature}`;
    this.hitlCardEl.classList.remove("hidden");
  }

  private extractCoords(action: AttributedUIAction): { x: number; y: number } | null {
    if (typeof action.endX === "number" && typeof action.endY === "number") {
      return { x: action.endX, y: action.endY };
    }
    if (typeof action.x === "number" && typeof action.y === "number") {
      return { x: action.x, y: action.y };
    }
    return null;
  }

  private showToast(
    phase: HudToastPhase,
    title: string,
    subtitle?: string,
    autoDismissMs?: number
  ): void {
    if (this.toastTimer) {
      window.clearTimeout(this.toastTimer);
      this.toastTimer = null;
    }
    this.toastPhase = phase;
    this.toastEl.dataset.phase = phase;
    this.toastTitleEl.textContent = title;
    this.toastSubEl.textContent = subtitle ?? "";
    this.toastSubEl.classList.toggle("hidden", !subtitle);
    this.toastEl.classList.remove("hidden");
    this.updateToastDock();

    if (autoDismissMs) {
      this.toastTimer = window.setTimeout(() => {
        this.toastPhase = "idle";
        this.toastEl.classList.add("hidden");
      }, autoDismissMs);
    }
  }

  private updateToastDock(): void {
    if (
      (this.toastPhase === "executing" || this.toastPhase === "awaitingConfirmation") &&
      this.activeReticle &&
      !this.activeReticle.centered
    ) {
      const endNy = this.activeReticle.y / Math.max(1, this.deviceHeight);
      const startNy =
        typeof this.activeReticle.startY === "number"
          ? this.activeReticle.startY / Math.max(1, this.deviceHeight)
          : endNy;
      const dockTop = Math.max(endNy, startNy) >= 0.7;
      this.toastEl.classList.toggle("dock-top", dockTop);
    } else {
      this.toastEl.classList.remove("dock-top");
    }
  }

  private setReticle(reticle: AgenticReticle, autoClearMs: number | null): void {
    if (this.reticleTimer) {
      window.clearTimeout(this.reticleTimer);
      this.reticleTimer = null;
    }
    this.activeReticle = reticle;
    this.updateToastDock();
    this.renderReticle();
    if (autoClearMs !== null) {
      this.scheduleReticleClear(autoClearMs);
    }
  }

  private scheduleReticleClear(ms: number): void {
    if (this.reticleTimer) window.clearTimeout(this.reticleTimer);
    this.reticleTimer = window.setTimeout(() => {
      this.activeReticle = null;
      this.renderReticle();
    }, ms);
  }

  public renderReticle(): void {
    this.svg.innerHTML = "";
    const r = this.activeReticle;
    if (!r) return;

    const bounds = this.stage.getBoundingClientRect();
    if (bounds.width <= 0 || bounds.height <= 0) return;

    const color =
      r.mode === "awaitingUserInput"
        ? "#FFCC00"
        : r.mode === "awaitingBiometrics"
          ? "#0A84FF"
          : "#FF3B30";

    const isSwipe =
      r.kind === "swipe" &&
      typeof r.startX === "number" &&
      typeof r.startY === "number" &&
      (r.startX !== r.x || r.startY !== r.y);

    if (isSwipe && typeof r.startX === "number" && typeof r.startY === "number") {
      const start = deviceToStagePoint(
        r.startX,
        r.startY,
        bounds.width,
        bounds.height,
        this.deviceWidth,
        this.deviceHeight
      );
      const end = deviceToStagePoint(
        r.x,
        r.y,
        bounds.width,
        bounds.height,
        this.deviceWidth,
        this.deviceHeight
      );
      const line = document.createElementNS(SVG_NS, "line");
      line.setAttribute("x1", start.x.toFixed(1));
      line.setAttribute("y1", start.y.toFixed(1));
      line.setAttribute("x2", end.x.toFixed(1));
      line.setAttribute("y2", end.y.toFixed(1));
      line.setAttribute("stroke", "#38BDF8");
      line.setAttribute("stroke-width", "3.2");
      line.setAttribute("stroke-linecap", "round");
      line.setAttribute("class", "hud-swipe-line");
      this.svg.appendChild(line);

      const dot = document.createElementNS(SVG_NS, "circle");
      dot.setAttribute("cx", start.x.toFixed(1));
      dot.setAttribute("cy", start.y.toFixed(1));
      dot.setAttribute("r", "6");
      dot.setAttribute("fill", "rgba(56,189,248,0.28)");
      dot.setAttribute("stroke", "#38BDF8");
      dot.setAttribute("stroke-width", "2");
      this.svg.appendChild(dot);

      const angle = Math.atan2(end.y - start.y, end.x - start.x);
      const wing = 11;
      const spread = Math.PI / 5.2;
      const w1x = end.x - wing * Math.cos(angle - spread);
      const w1y = end.y - wing * Math.sin(angle - spread);
      const w2x = end.x - wing * Math.cos(angle + spread);
      const w2y = end.y - wing * Math.sin(angle + spread);
      const chevron = document.createElementNS(SVG_NS, "path");
      chevron.setAttribute(
        "d",
        `M ${w1x.toFixed(1)} ${w1y.toFixed(1)} L ${end.x.toFixed(1)} ${end.y.toFixed(1)} L ${w2x.toFixed(1)} ${w2y.toFixed(1)}`
      );
      chevron.setAttribute("fill", "none");
      chevron.setAttribute("stroke", "#FFFFFF");
      chevron.setAttribute("stroke-width", "2.6");
      chevron.setAttribute("stroke-linecap", "round");
      chevron.setAttribute("stroke-linejoin", "round");
      this.svg.appendChild(chevron);
      return;
    }

    const pt = r.centered
      ? { x: bounds.width / 2, y: bounds.height / 2 }
      : deviceToStagePoint(
          r.x,
          r.y,
          bounds.width,
          bounds.height,
          this.deviceWidth,
          this.deviceHeight
        );

    const g = document.createElementNS(SVG_NS, "g");
    g.setAttribute("transform", `translate(${pt.x.toFixed(1)}, ${pt.y.toFixed(1)})`);

    const halo = document.createElementNS(SVG_NS, "circle");
    halo.setAttribute("r", "20");
    halo.setAttribute("fill", `${color}28`);
    halo.setAttribute("stroke", color);
    halo.setAttribute("stroke-width", "2");
    halo.setAttribute("class", "hud-reticle-halo");
    g.appendChild(halo);

    const ring = document.createElementNS(SVG_NS, "circle");
    ring.setAttribute("r", "13");
    ring.setAttribute("fill", `${color}1C`);
    ring.setAttribute("stroke", color);
    ring.setAttribute("stroke-width", "2.2");
    ring.setAttribute("class", "hud-reticle-ring");
    g.appendChild(ring);

    const ticks = document.createElementNS(SVG_NS, "path");
    ticks.setAttribute("d", "M -21 0 L -7 0 M 7 0 L 21 0 M 0 -21 L 0 -7 M 0 7 L 0 21");
    ticks.setAttribute("stroke", color);
    ticks.setAttribute("stroke-width", "2");
    ticks.setAttribute("stroke-linecap", "round");
    g.appendChild(ticks);

    const centerDot = document.createElementNS(SVG_NS, "circle");
    centerDot.setAttribute("r", "3.2");
    centerDot.setAttribute("fill", color);
    centerDot.setAttribute("stroke", "#FFFFFF");
    centerDot.setAttribute("stroke-width", "0.9");
    g.appendChild(centerDot);

    this.svg.appendChild(g);
  }
}
