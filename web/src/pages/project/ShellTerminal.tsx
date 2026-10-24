import { FitAddon } from "@xterm/addon-fit";
import { type ITheme, Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useEffect, useRef } from "react";
import { execUrl } from "@/lib/api";
import { decodeFrame, exitLine, resizeFrame } from "./shell";

/** A CSS color (oklch from the tokens) as rgba(), which xterm.js parses. */
function rgba(ctx: CanvasRenderingContext2D | null, css: string, fallback: string): string {
  if (!ctx || !css) return fallback;
  ctx.clearRect(0, 0, 1, 1);
  ctx.fillStyle = fallback;
  ctx.fillStyle = css;
  ctx.fillRect(0, 0, 1, 1);
  const [r = 0, g = 0, b = 0, a = 255] = ctx.getImageData(0, 0, 1, 1).data;
  return `rgba(${r}, ${g}, ${b}, ${(a / 255).toFixed(3)})`;
}

/** The terminal's colors from the dark theme tokens of `el` (a `.dark` element). */
function themeFrom(el: HTMLElement): ITheme {
  const s = getComputedStyle(el);
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 1;
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  const c = (name: string, fallback: string) => rgba(ctx, s.getPropertyValue(name).trim(), fallback);
  const fg = c("--foreground", "#fafafa");
  const muted = c("--muted-foreground", "#a3a3a3");
  const red = c("--error", "#f87171");
  const green = c("--success", "#4ade80");
  const yellow = c("--warning", "#facc15");
  const blue = c("--info", "#60a5fa");
  return {
    background: c("--card", "#1c1c1c"),
    foreground: fg,
    cursor: fg,
    cursorAccent: c("--card", "#1c1c1c"),
    selectionBackground: c("--input", "rgba(255,255,255,0.15)"),
    black: c("--muted", "#262626"),
    brightBlack: muted,
    red,
    brightRed: red,
    green,
    brightGreen: green,
    yellow,
    brightYellow: yellow,
    blue,
    brightBlue: blue,
    magenta: c("--badge-violet-fg", "#e5e5e5"),
    brightMagenta: c("--badge-violet-fg", "#e5e5e5"),
    cyan: c("--badge-blue-fg", "#93c5fd"),
    brightCyan: c("--badge-blue-fg", "#93c5fd"),
    white: muted,
    brightWhite: fg,
  };
}

/** An interactive terminal attached to a shell in `deploymentId`'s container
 *  (the exec WebSocket). Loaded on demand: xterm.js is only needed here. */
export default function ShellTerminal({
  deploymentId,
  onOpen,
  onClose,
}: {
  deploymentId: string;
  onOpen: () => void;
  onClose: (opened: boolean) => void;
}) {
  const box = useRef<HTMLDivElement>(null);
  // Callbacks change every render; the session must not restart for that.
  const handlers = useRef({ onOpen, onClose });
  handlers.current = { onOpen, onClose };

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const term = new Terminal({
      cursorBlink: true,
      fontFamily: getComputedStyle(el).getPropertyValue("--app-font-mono").trim() || "monospace",
      fontSize: 13,
      scrollback: 5000,
      theme: themeFrom(el),
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(el);
    fit.fit();

    const ws = new WebSocket(execUrl(deploymentId, { cols: term.cols, rows: term.rows }));
    ws.binaryType = "arraybuffer";
    const enc = new TextEncoder();
    let opened = false;
    let ended = false;
    const note = (text: string) => term.write(`\r\n\x1b[2m${text}\x1b[0m\r\n`);
    ws.onopen = () => {
      opened = true;
      handlers.current.onOpen();
      term.focus();
    };
    ws.onmessage = (e: MessageEvent<ArrayBuffer | string>) => {
      const f = decodeFrame(e.data);
      if (!f) return;
      if (f.kind === "output") {
        term.write(f.bytes);
      } else {
        ended = true;
        note(exitLine(f.code, f.reason));
      }
    };
    ws.onclose = () => {
      if (!opened) note("Could not start a shell: the deployment may have stopped, or you lack access.");
      else if (!ended) note("Disconnected.");
      handlers.current.onClose(opened);
    };
    const typed = term.onData((d) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(enc.encode(d));
    });
    const resized = term.onResize(({ cols, rows }) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(resizeFrame(cols, rows));
    });
    const observer = new ResizeObserver(() => fit.fit());
    observer.observe(el);
    return () => {
      observer.disconnect();
      typed.dispose();
      resized.dispose();
      ws.onclose = null;
      ws.close();
      term.dispose();
    };
  }, [deploymentId]);

  return <div ref={box} className="dark h-[min(60vh,560px)] min-h-[320px] w-full overflow-hidden rounded-lg border border-border bg-card p-2" />;
}
