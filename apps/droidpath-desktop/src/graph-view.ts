import type { ScreenStateVertex, SpatialGraphSnapshot } from "./types";

const SVG_NS = "http://www.w3.org/2000/svg";

function packageColor(pkg: string): string {
  let hash = 0;
  for (let i = 0; i < pkg.length; i++) {
    hash = (hash * 31 + pkg.charCodeAt(i)) | 0;
  }
  const palette = ["#38BDF8", "#A78BFA", "#34D399", "#F472B6", "#FB923C", "#FACC15", "#60A5FA"];
  return palette[Math.abs(hash) % palette.length];
}

function vertexTitle(v: ScreenStateVertex): string {
  if (v.sampleInteractiveLabels.length > 0 && v.sampleInteractiveLabels[0].trim()) {
    return v.sampleInteractiveLabels[0].slice(0, 16);
  }
  const seg = v.activePackage.split(".").pop() || "Screen";
  return seg.slice(0, 16);
}

/** Lightweight deterministic layered SVG renderer for the Spatial Graph. */
export function renderSpatialGraphSvg(
  svg: SVGSVGElement,
  emptyEl: HTMLElement,
  snapshot: SpatialGraphSnapshot,
  onSelectVertex: (vertex: ScreenStateVertex) => void
): void {
  svg.innerHTML = "";
  const vertices = snapshot.vertices;
  emptyEl.classList.toggle("hidden", vertices.length > 0);
  if (vertices.length === 0) return;

  const width = 420;
  const height = 280;
  const paddingX = 48;
  const paddingY = 34;

  // Group vertices by discoveryDepth for clean left-to-right DAG layout
  const byDepth = new Map<number, ScreenStateVertex[]>();
  let maxDepth = 0;
  for (const v of vertices) {
    const d = Math.max(0, v.discoveryDepth);
    maxDepth = Math.max(maxDepth, d);
    const list = byDepth.get(d) ?? [];
    list.push(v);
    byDepth.set(d, list);
  }

  const positions = new Map<string, { x: number; y: number }>();
  const depthCount = Math.max(1, maxDepth + 1);
  for (const [depth, list] of byDepth.entries()) {
    const x =
      depthCount === 1
        ? width / 2
        : paddingX + (depth / (depthCount - 1)) * (width - paddingX * 2);
    list.forEach((v, idx) => {
      const y =
        list.length === 1
          ? height / 2
          : paddingY + (idx / (list.length - 1)) * (height - paddingY * 2);
      positions.set(v.layoutHashHex, { x, y });
    });
  }

  // Arrowhead marker defs
  const defs = document.createElementNS(SVG_NS, "defs");
  defs.innerHTML = `
    <marker id="dp-arrow" viewBox="0 0 10 10" refX="14" refY="5" markerWidth="5" markerHeight="5" orient="auto-start-reverse">
      <path d="M 0 1 L 10 5 L 0 9 z" fill="#64748B" />
    </marker>
    <marker id="dp-arrow-warn" viewBox="0 0 10 10" refX="14" refY="5" markerWidth="5" markerHeight="5" orient="auto-start-reverse">
      <path d="M 0 1 L 10 5 L 0 9 z" fill="#F59E0B" />
    </marker>
  `;
  svg.appendChild(defs);

  // Draw edges
  for (const edge of snapshot.edges) {
    const from = positions.get(edge.fromHashHex);
    const to = positions.get(edge.toHashHex);
    if (!from || !to) continue;
    const line = document.createElementNS(SVG_NS, "line");
    line.setAttribute("x1", from.x.toFixed(1));
    line.setAttribute("y1", from.y.toFixed(1));
    line.setAttribute("x2", to.x.toFixed(1));
    line.setAttribute("y2", to.y.toFixed(1));
    const isSeedEdge = Boolean(edge.isSeed);
    line.setAttribute(
      "stroke",
      edge.isHazardous
        ? "#F59E0B"
        : isSeedEdge
          ? "rgba(148,163,184,0.28)"
          : "rgba(148,163,184,0.52)"
    );
    line.setAttribute("stroke-width", edge.isHazardous ? "1.8" : isSeedEdge ? "1.1" : "1.4");
    if (edge.isHazardous) {
      line.setAttribute("stroke-dasharray", "4 3");
    } else if (isSeedEdge) {
      line.setAttribute("stroke-dasharray", "3 3");
    }
    line.setAttribute("marker-end", edge.isHazardous ? "url(#dp-arrow-warn)" : "url(#dp-arrow)");
    const title = document.createElementNS(SVG_NS, "title");
    title.textContent = edge.isHazardous
      ? `⚠️ ${edge.actionSignature} (${edge.hazardReason ?? "hazardous"})`
      : isSeedEdge
        ? `${edge.actionSignature} (seed — hydrates on 1st traversal)`
        : `${edge.actionSignature} (${edge.traversalCount}x)`;
    line.appendChild(title);
    svg.appendChild(line);
  }

  // Draw vertices
  for (const v of vertices) {
    const pos = positions.get(v.layoutHashHex);
    if (!pos) continue;
    const isCurrent = snapshot.currentStateHashHex === v.layoutHashHex;
    const color = packageColor(v.activePackage);

    const g = document.createElementNS(SVG_NS, "g");
    g.setAttribute("class", "graph-node");
    g.setAttribute("transform", `translate(${pos.x.toFixed(1)}, ${pos.y.toFixed(1)})`);
    g.style.cursor = "pointer";
    g.addEventListener("click", () => onSelectVertex(v));

    if (isCurrent) {
      const glow = document.createElementNS(SVG_NS, "circle");
      glow.setAttribute("r", "15");
      glow.setAttribute("fill", "none");
      glow.setAttribute("stroke", "#FF3B30");
      glow.setAttribute("stroke-width", "2");
      glow.setAttribute("opacity", "0.85");
      g.appendChild(glow);
    }

    const isSeedNode = Boolean(v.isSeed);
    const circle = document.createElementNS(SVG_NS, "circle");
    circle.setAttribute("r", isCurrent ? "10" : isSeedNode ? "6.5" : "8");
    circle.setAttribute("fill", isSeedNode ? "rgba(56,189,248,0.22)" : color);
    circle.setAttribute("stroke", isSeedNode ? color : "#0B0D12");
    circle.setAttribute("stroke-width", isSeedNode ? "1.5" : "2");
    if (isSeedNode) {
      circle.setAttribute("stroke-dasharray", "3 2");
    }
    g.appendChild(circle);

    const text = document.createElementNS(SVG_NS, "text");
    text.setAttribute("y", "21");
    text.setAttribute("text-anchor", "middle");
    text.setAttribute("fill", isSeedNode ? "#94A3B8" : "#CBD5E1");
    text.setAttribute("font-size", "9");
    text.textContent = vertexTitle(v);
    g.appendChild(text);

    const tooltip = document.createElementNS(SVG_NS, "title");
    const seedTag = isSeedNode ? "\nStatus: Seed (auto-hydrates on 1st traversal)" : "";
    tooltip.textContent = `${v.activePackage}${seedTag}\nLabels: ${v.sampleInteractiveLabels.join(", ")}\nVisits: ${v.visitCount}\nClick to route here via A*`;
    g.appendChild(tooltip);

    svg.appendChild(g);
  }
}
