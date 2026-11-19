export interface DroidPathDevice {
  serial: string;
  model: string;
  is_wireless: boolean;
}

export interface ScrcpySessionInfo {
  active: boolean;
  serial: string;
  wsUrl: string;
  wsToken: string;
  videoWidth: number;
  videoHeight: number;
  scrcpyVersion: string;
}

export interface ConnectDeviceResult {
  serial: string;
  daemonReady: boolean;
  daemonWarning?: string | null;
  mirror?: ScrcpySessionInfo | null;
  mirrorMode: "webcodecs" | "snapshot" | string;
  mirrorWarning?: string | null;
  videoWidth: number;
  videoHeight: number;
  displayWidth: number;
  displayHeight: number;
}

export interface DaemonConnectionState {
  status: "disconnected" | "connecting" | "connected" | "waiting_to_reconnect" | "stopped";
  attempt?: number;
}

export interface DroidPathGraphStats {
  vertexCount: number;
  edgeCount: number;
  currentStateHashHex: string | null;
  currentPackage: string;
  connectionState: DaemonConnectionState;
  hasFreshShadowDom: boolean;
  storagePath: string;
}

export interface AttributedUIAction {
  kind: "tap" | "swipe" | "keyevent" | "typeText" | "launchApp";
  targetDescription: string;
  x?: number | null;
  y?: number | null;
  endX?: number | null;
  endY?: number | null;
  durationMs?: number | null;
  selectorResourceId?: string | null;
  selectorText?: string | null;
  selectorContentDesc?: string | null;
  timestamp: string;
}

export interface HazardousConfirmationRequest {
  stepIndex: number;
  totalSteps: number;
  actionSignature: string;
  targetDescription: string;
  hazardCategory: "payment" | "destructive" | "externalSideEffect";
  hazardReason: string;
  currentHash: number;
  targetHash: number;
  stepsCompleted: number;
  tapX?: number | null;
  tapY?: number | null;
}

export interface MacroStepReport {
  stepIndex: number;
  edgeId: string;
  actionSignature: string;
  targetHash: number;
  elapsedMs: number;
  matchedByExactHash: boolean;
  wasSkipped: boolean;
}

export interface MacroExecutionReport {
  startHash: number;
  finalHash: number;
  steps: MacroStepReport[];
  totalElapsedMs: number;
  recoveredSteps: string[];
}

export type MacroExecutionEvent =
  | { type: "pathComputed"; totalSteps: number; targetHint?: string | null }
  | { type: "executionStarted"; totalSteps: number }
  | {
      type: "willExecuteAction";
      stepIndex: number;
      totalSteps: number;
      action: AttributedUIAction;
    }
  | {
      type: "stepSettled";
      stepIndex: number;
      totalSteps: number;
      report: MacroStepReport;
    }
  | { type: "completed"; report: MacroExecutionReport }
  | { type: "failed"; message: string }
  | {
      type: "awaitingConfirmation";
      stepIndex: number;
      totalSteps: number;
      action: AttributedUIAction;
      reason: string;
      request: HazardousConfirmationRequest;
    }
  | { type: "awaitingChatInput"; prompt: string; focusX?: number | null; focusY?: number | null }
  | { type: "awaitingBiometrics"; reason: string; activePackage: string }
  | { type: "biometricsCleared" }
  | { type: "recoveryAction"; kind: string; detail: string };

export interface WireSecureScreenPayload {
  code: string;
  active: boolean;
  reason: string;
  package: string;
  ts_ms: number;
}

export interface PassiveLearningEvent {
  kind: string;
  label: string;
  x?: number | null;
  y?: number | null;
  selectorText?: string | null;
  selectorResourceId?: string | null;
}

export interface ScreenStateVertex {
  layoutHashHex: string;
  activePackage: string;
  sampleInteractiveLabels: string[];
  visitCount: number;
  discoveryParentHashHex?: string | null;
  discoveryDepth: number;
}

export interface ScreenTransitionEdge {
  id: string;
  fromHashHex: string;
  toHashHex: string;
  actionSignature: string;
  traversalCount: number;
  isHazardous: boolean;
  hazardReason?: string | null;
}

export interface SpatialGraphSnapshot {
  rootStateHashHex?: string | null;
  currentStateHashHex?: string | null;
  vertices: ScreenStateVertex[];
  edges: ScreenTransitionEdge[];
}

export interface McpServerStatus {
  running: boolean;
  port?: number | null;
  sseUrl?: string | null;
}
