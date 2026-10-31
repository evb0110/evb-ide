/** @jsxRuntime automatic */
/** @jsxImportSource react */
// The landing page film: states of the real app recorded by `evb-visual-capture` (EVB_FILM_DIR),
// sequenced with a pointer, click ripples, a camera and key presses. Adapted from the EVB Viewer
// landing film. render.mjs renders it to the videos the page plays.
import { Composition, Easing, Img, interpolate, registerRoot, staticFile, useCurrentFrame } from "remotion";

type TRect = [number, number, number, number];

export interface IFilmStep {
  image: string;
  frames: number;
  cursor: [number, number] | null;
  /** The pointer clicks at the end of the step; the next step shows the result. */
  click: boolean;
  /** The part of the window the camera frames. */
  focus: TRect | null;
  keys: string | null;
}

export interface IFilmManifest {
  width: number;
  height: number;
  fps: number;
  steps: IFilmStep[];
}

interface IFilmProps extends Record<string, unknown> {
  manifest: IFilmManifest;
  theme: "light" | "dark";
}

const FADE = 8;
const LOOP_OUT = 12;
const LOOP_IN = 10;
const CURSOR_MOVE = 18;
const CAMERA_MOVE = 22;

function lerpRect(a: TRect, b: TRect, t: number): TRect {
  return [0, 1, 2, 3].map((i) => a[i]! + (b[i]! - a[i]!) * t) as TRect;
}

function Pointer({ x, y, pressed }: { x: number; y: number; pressed: number }) {
  return (
    <svg
      width={22}
      height={22}
      viewBox="0 0 28 28"
      style={{
        position: "absolute",
        left: x - 2,
        top: y - 2,
        overflow: "visible",
        transform: `scale(${1 - pressed * 0.12})`,
        transformOrigin: "3px 2px",
      }}
    >
      <path
        d="M3 2 L3 22 L8.5 17 L12.5 26 L16 24.5 L12 15.8 L19.5 15.5 Z"
        fill="#111"
        stroke="#fff"
        strokeWidth={1.6}
        strokeLinejoin="round"
      />
    </svg>
  );
}

function Keys({ keys, opacity, dark }: { keys: string; opacity: number; dark: boolean }) {
  return (
    <div
      style={{
        position: "absolute",
        left: "50%",
        bottom: 56,
        transform: "translateX(-50%)",
        padding: "10px 18px",
        borderRadius: 10,
        font: "600 26px/1 Inter, system-ui, sans-serif",
        letterSpacing: 2,
        color: dark ? "#f4f4f5" : "#18181b",
        background: dark ? "rgba(39, 39, 42, 0.92)" : "rgba(255, 255, 255, 0.94)",
        border: `1px solid ${dark ? "rgba(255,255,255,0.14)" : "rgba(24,24,27,0.14)"}`,
        boxShadow: "0 12px 30px -10px rgba(0,0,0,0.35)",
        opacity,
      }}
    >
      {keys}
    </div>
  );
}

export function Film({ manifest: m, theme }: IFilmProps) {
  const frame = useCurrentFrame();
  const starts: number[] = [];
  let total = 0;
  for (const step of m.steps) {
    starts.push(total);
    total += step.frames;
  }
  const full: TRect = [0, 0, m.width, m.height];
  const cursorAt: Array<[number, number] | null> = [];
  m.steps.forEach((step, i) => cursorAt.push(step.cursor ?? cursorAt[i - 1] ?? null));

  let i = starts.findIndex((start, k) => frame >= start && frame < start + m.steps[k]!.frames);
  if (i < 0) i = m.steps.length - 1;
  const step = m.steps[i]!;
  const local = frame - starts[i]!;
  const prev = i > 0 ? m.steps[i - 1]! : null;

  // A new state crossfades in; the same state carries on.
  const changes = prev !== null && prev.image !== step.image;
  const curOpacityBase = changes ? interpolate(local, [0, FADE], [0, 1], { extrapolateRight: "clamp" }) : 1;
  const loopOut = interpolate(frame, [total - LOOP_OUT, total], [1, 0], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
  });
  const loopIn = interpolate(frame, [0, LOOP_IN], [0, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp" });
  const curOpacity = curOpacityBase * loopOut * (i === 0 ? loopIn : 1);

  const settle = interpolate(local, [0, CAMERA_MOVE], [0, 1], {
    extrapolateRight: "clamp",
    easing: Easing.inOut(Easing.cubic),
  });
  const cam = lerpRect(prev?.focus ?? full, step.focus ?? full, prev ? settle : 1);
  const scale = Math.min(m.width / cam[2], m.height / cam[3]);
  const tx = -cam[0] * scale + (m.width - cam[2] * scale) / 2;
  const ty = -cam[1] * scale + (m.height - cam[3] * scale) / 2;

  const target = cursorAt[i];
  const from = i > 0 ? cursorAt[i - 1] : null;
  let pointer: [number, number] | null = null;
  if (target) {
    const t = interpolate(local, [0, CURSOR_MOVE], [0, 1], {
      extrapolateRight: "clamp",
      easing: Easing.inOut(Easing.cubic),
    });
    pointer = from ? [from[0] + (target[0] - from[0]) * t, from[1] + (target[1] - from[1]) * t] : target;
  }
  const clickT = step.click
    ? interpolate(local, [step.frames - 8, step.frames], [0, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp" })
    : 0;
  const pressed = step.click ? Math.sin(Math.min(1, clickT) * Math.PI) : 0;
  const ripple =
    prev?.click === true ? interpolate(local, [0, 14], [0, 1], { extrapolateRight: "clamp" }) : 0;
  const rippleAt = i > 0 ? cursorAt[i - 1] : null;
  const keysOpacity = step.keys
    ? interpolate(local, [0, 6, step.frames - 10, step.frames], [0, 1, 1, 0], {
        extrapolateLeft: "clamp",
        extrapolateRight: "clamp",
      })
    : 0;
  const dark = theme === "dark";
  const image = (s: IFilmStep, opacity: number, key: string) => (
    <Img
      key={key}
      src={staticFile(`${theme}/${s.image}.png`)}
      style={{ position: "absolute", left: 0, top: 0, width: m.width, height: m.height, opacity }}
    />
  );

  return (
    <div style={{ position: "absolute", inset: 0, overflow: "hidden", background: dark ? "#1e1f22" : "#ffffff" }}>
      <div
        style={{
          position: "absolute",
          left: 0,
          top: 0,
          width: m.width,
          height: m.height,
          transform: `translate(${tx}px, ${ty}px) scale(${scale})`,
          transformOrigin: "0 0",
        }}
      >
        {prev && changes && curOpacityBase < 1 && image(prev, loopOut, `p${i}`)}
        {image(step, curOpacity, `s${i}`)}
        {ripple > 0 && ripple < 1 && rippleAt && (
          <div
            style={{
              position: "absolute",
              left: rippleAt[0] - 16,
              top: rippleAt[1] - 16,
              width: 32,
              height: 32,
              borderRadius: "50%",
              border: "2px solid rgba(53, 116, 240, 0.9)",
              transform: `scale(${0.4 + ripple})`,
              opacity: 1 - ripple,
            }}
          />
        )}
        {pointer && loopOut > 0.5 && (i > 0 || loopIn > 0.5) && (
          <Pointer x={pointer[0]} y={pointer[1]} pressed={pressed} />
        )}
      </div>
      {step.keys && keysOpacity > 0 && <Keys keys={step.keys} opacity={keysOpacity} dark={dark} />}
    </div>
  );
}

const PLACEHOLDER: IFilmManifest = { width: 1440, height: 900, fps: 30, steps: [] };

function Root() {
  return (
    <Composition
      id="film"
      component={Film}
      defaultProps={{ manifest: PLACEHOLDER, theme: "light" }}
      fps={30}
      width={1440}
      height={900}
      durationInFrames={1}
      calculateMetadata={({ props }) => ({
        fps: props.manifest.fps,
        width: props.manifest.width,
        height: props.manifest.height,
        durationInFrames: Math.max(1, props.manifest.steps.reduce((sum, step) => sum + step.frames, 0)),
      })}
    />
  );
}

registerRoot(Root);
