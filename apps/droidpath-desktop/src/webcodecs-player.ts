const WS_FRAME_META = 0x01;
const WS_FRAME_VIDEO = 0x02;
const VIDEO_FLAG_CONFIG = 1 << 0;
const VIDEO_FLAG_KEYFRAME = 1 << 1;

export interface WebCodecsPlayerCallbacks {
  onDimensions?: (width: number, height: number) => void;
  onFps?: (fps: number) => void;
  onStatus?: (status: string) => void;
  onError?: (err: string) => void;
}

export function isWebCodecsSupported(): boolean {
  return typeof window !== "undefined" && "VideoDecoder" in window && "EncodedVideoChunk" in window;
}

/** Splits an Annex-B byte buffer (`00 00 01` or `00 00 00 01` start codes) into raw NAL units. */
export function splitAnnexBNalUnits(data: Uint8Array): Uint8Array[] {
  const nals: Uint8Array[] = [];
  const starts: Array<{ index: number; len: number }> = [];
  for (let i = 0; i + 3 <= data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0) {
      if (data[i + 2] === 1) {
        starts.push({ index: i, len: 3 });
        i += 2;
      } else if (i + 4 <= data.length && data[i + 2] === 0 && data[i + 3] === 1) {
        starts.push({ index: i, len: 4 });
        i += 3;
      }
    }
  }
  if (starts.length === 0 && data.length > 0) {
    return [data];
  }
  for (let s = 0; s < starts.length; s++) {
    const startOffset = starts[s].index + starts[s].len;
    const endOffset = s + 1 < starts.length ? starts[s + 1].index : data.length;
    if (endOffset > startOffset) {
      nals.push(data.subarray(startOffset, endOffset));
    }
  }
  return nals;
}

/** Builds an ISO 14496-15 `avcC` AVCDecoderConfigurationRecord from raw SPS and PPS NALUs. */
export function buildAvccDescription(sps: Uint8Array, pps: Uint8Array): {
  codec: string;
  description: Uint8Array;
} {
  const profileIdc = sps.length > 1 ? sps[1] : 0x42;
  const profileCompat = sps.length > 2 ? sps[2] : 0xe0;
  const levelIdc = sps.length > 3 ? sps[3] : 0x1e;
  const hex2 = (n: number) => n.toString(16).padStart(2, "0").toUpperCase();
  const codec = `avc1.${hex2(profileIdc)}${hex2(profileCompat)}${hex2(levelIdc)}`;

  const totalLen = 6 + 2 + sps.length + 1 + 2 + pps.length;
  const avcc = new Uint8Array(totalLen);
  let o = 0;
  avcc[o++] = 1; // configurationVersion
  avcc[o++] = profileIdc;
  avcc[o++] = profileCompat;
  avcc[o++] = levelIdc;
  avcc[o++] = 0xff; // 4-byte NALU length size (3 + 1)
  avcc[o++] = 0xe1; // 1 SPS
  avcc[o++] = (sps.length >> 8) & 0xff;
  avcc[o++] = sps.length & 0xff;
  avcc.set(sps, o);
  o += sps.length;
  avcc[o++] = 1; // 1 PPS
  avcc[o++] = (pps.length >> 8) & 0xff;
  avcc[o++] = pps.length & 0xff;
  avcc.set(pps, o);

  return { codec, description: avcc };
}

/** Packs NAL units into 4-byte big-endian length-prefixed AVCC payload for WebCodecs VideoDecoder. */
export function packAvccFrame(nals: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const nal of nals) {
    total += 4 + nal.length;
  }
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  let offset = 0;
  for (const nal of nals) {
    view.setUint32(offset, nal.length, false);
    offset += 4;
    out.set(nal, offset);
    offset += nal.length;
  }
  return out;
}

export function bytesEqual(a: Uint8Array | null, b: Uint8Array | null): boolean {
  if (a === b) return true;
  if (!a || !b || a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

export interface ParsedH264Packet {
  sps: Uint8Array | null;
  pps: Uint8Array | null;
  sliceNals: Uint8Array[];
  hasIdr: boolean;
  hasVclSlice: boolean;
}

/** Extracts SPS/PPS parameter sets and decodable VCL/SEI NALs from an Annex-B packet. */
export function parseH264AnnexBPacket(payload: Uint8Array, isKeyFlag = false): ParsedH264Packet {
  const nals = splitAnnexBNalUnits(payload);
  let sps: Uint8Array | null = null;
  let pps: Uint8Array | null = null;
  const sliceNals: Uint8Array[] = [];
  let hasIdr = isKeyFlag;
  let hasVclSlice = false;

  for (const nal of nals) {
    if (nal.length === 0) continue;
    const nalType = nal[0] & 0x1f;
    if (nalType === 7) {
      sps = nal.slice();
    } else if (nalType === 8) {
      pps = nal.slice();
    } else if (nalType === 5) {
      hasIdr = true;
      hasVclSlice = true;
      sliceNals.push(nal);
    } else if (nalType === 1) {
      hasVclSlice = true;
      sliceNals.push(nal);
    } else if (nalType === 6) {
      sliceNals.push(nal);
    }
  }

  return { sps, pps, sliceNals, hasIdr, hasVclSlice };
}

/**
 * Low-latency H.264 -> WebCodecs VideoDecoder player over local loopback WebSocket.
 * Works across Windows (WebView2), macOS (WKWebView), and Linux (WebKitGTK/Chromium).
 */
export class WebCodecsH264Player {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D | null;
  private callbacks: WebCodecsPlayerCallbacks;
  private ws: WebSocket | null = null;
  private decoder: VideoDecoder | null = null;
  private sps: Uint8Array | null = null;
  private pps: Uint8Array | null = null;
  private configured = false;
  private waitingForKey = true;
  private frameCounter = 0;
  private fpsTimer: number | null = null;

  constructor(canvas: HTMLCanvasElement, callbacks: WebCodecsPlayerCallbacks = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
    this.callbacks = callbacks;
  }

  private createDecoder(): VideoDecoder {
    return new VideoDecoder({
      output: (frame: VideoFrame) => {
        if (this.canvas.width !== frame.displayWidth || this.canvas.height !== frame.displayHeight) {
          this.canvas.width = frame.displayWidth;
          this.canvas.height = frame.displayHeight;
          this.callbacks.onDimensions?.(frame.displayWidth, frame.displayHeight);
        }
        this.ctx?.drawImage(frame, 0, 0, this.canvas.width, this.canvas.height);
        frame.close();
        this.frameCounter++;
      },
      error: (err: DOMException) => {
        this.callbacks.onError?.(`VideoDecoder error: ${err.message}`);
        this.configured = false;
        this.waitingForKey = true;
      },
    });
  }

  public start(wsUrl: string): void {
    this.stop();
    if (!isWebCodecsSupported()) {
      this.callbacks.onError?.("WebCodecs VideoDecoder is not supported in this WebView");
      return;
    }

    this.decoder = this.createDecoder();

    const ws = new WebSocket(wsUrl);
    ws.binaryType = "arraybuffer";
    this.ws = ws;

    ws.onopen = () => {
      this.callbacks.onStatus?.("WebCodecs H.264 Connected");
    };
    ws.onmessage = (event: MessageEvent) => {
      if (event.data instanceof ArrayBuffer) {
        this.handleBinaryFrame(new Uint8Array(event.data));
      }
    };
    ws.onerror = () => {
      this.callbacks.onError?.("Video stream WebSocket error");
    };
    ws.onclose = () => {
      this.callbacks.onStatus?.("Stream Closed");
    };

    this.fpsTimer = window.setInterval(() => {
      this.callbacks.onFps?.(this.frameCounter);
      this.frameCounter = 0;
    }, 1000);
  }

  public stop(): void {
    if (this.fpsTimer !== null) {
      window.clearInterval(this.fpsTimer);
      this.fpsTimer = null;
    }
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.onerror = null;
      this.ws.onmessage = null;
      this.ws.close();
      this.ws = null;
    }
    if (this.decoder) {
      try {
        if (this.decoder.state !== "closed") {
          this.decoder.close();
        }
      } catch {
        // Ignore teardown race
      }
      this.decoder = null;
    }
    this.sps = null;
    this.pps = null;
    this.configured = false;
    this.waitingForKey = true;
    this.frameCounter = 0;
  }

  private handleBinaryFrame(bytes: Uint8Array): void {
    if (bytes.length < 2) return;
    const frameType = bytes[0];
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);

    if (frameType === WS_FRAME_META && bytes.length >= 9) {
      const width = view.getUint32(1, false);
      const height = view.getUint32(5, false);
      if (width > 0 && height > 0) {
        this.canvas.width = width;
        this.canvas.height = height;
        this.callbacks.onDimensions?.(width, height);
      }
      return;
    }

    if (frameType !== WS_FRAME_VIDEO || bytes.length <= 10 || !this.decoder) {
      return;
    }

    const flags = bytes[1];
    const isConfig = (flags & VIDEO_FLAG_CONFIG) !== 0;
    const isKeyFlag = (flags & VIDEO_FLAG_KEYFRAME) !== 0;
    const ptsHi = view.getUint32(2, false);
    const ptsLo = view.getUint32(6, false);
    const ptsUs = ptsHi * 4294967296 + ptsLo;
    const payload = bytes.subarray(10);

    const parsed = parseH264AnnexBPacket(payload, isKeyFlag);
    let paramsChanged = false;
    if (parsed.sps && !bytesEqual(this.sps, parsed.sps)) {
      this.sps = parsed.sps;
      paramsChanged = true;
    }
    if (parsed.pps && !bytesEqual(this.pps, parsed.pps)) {
      this.pps = parsed.pps;
      paramsChanged = true;
    }

    // Recover transparently if an earlier async decode error transitioned VideoDecoder to "closed"
    if (this.decoder.state === "closed") {
      this.decoder = this.createDecoder();
      this.configured = false;
      this.waitingForKey = true;
    }

    const needsConfigure = !this.configured || paramsChanged || (isConfig && !this.configured);
    if (needsConfigure && this.sps && this.pps) {
      const { codec, description } = buildAvccDescription(this.sps, this.pps);
      try {
        this.decoder.configure({
          codec,
          description,
          optimizeForLatency: true,
        });
        this.configured = true;
        this.waitingForKey = true;
      } catch (err) {
        this.callbacks.onError?.(`Failed to configure VideoDecoder (${codec}): ${String(err)}`);
        return;
      }
    }

    // Never feed SEI-only packets without an actual VCL slice (NAL type 1 or 5)
    if (!this.configured || !parsed.hasVclSlice || parsed.sliceNals.length === 0) {
      return;
    }
    if (this.waitingForKey && !parsed.hasIdr) {
      return;
    }
    if (parsed.hasIdr) {
      this.waitingForKey = false;
    }

    const avccData = packAvccFrame(parsed.sliceNals);
    try {
      this.decoder.decode(
        new EncodedVideoChunk({
          type: parsed.hasIdr ? "key" : "delta",
          timestamp: ptsUs,
          data: avccData,
        })
      );
    } catch {
      this.waitingForKey = true;
    }
  }
}
