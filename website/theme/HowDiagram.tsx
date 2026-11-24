import { Zap } from "lucide-react";
import { useEffect, useState } from "react";
import {
  type Caption,
  type End,
  type PartKind,
  SCENE,
  type SceneNote,
  type ScenePart,
  type SceneView,
  type Side,
} from "./HowScene";

// A line drawing of how Gage works: who the actors are and what passes
// between them. It is drawn with strokes only, in the page's text
// color, so it follows light and dark mode.
//
// Each kind of part is drawn once here, in a box whose top left corner
// is 0,0. HowScene.ts says where each part goes, which parts are joined
// and in which frame of which view each thing appears. Everything is
// always in the drawing. What a frame shows is set by a data-state on
// each group, and Landing.css fades between the states.

// The size of each part's box.
const SIZES: Record<PartKind, { w: number; h: number }> = {
  you: { w: 68, h: 95 },
  agent: { w: 210, h: 130 },
  model: { w: 132, h: 62 },
  gage: { w: 160, h: 120 },
  logs: { w: 94, h: 104 },
  issues: { w: 94, h: 104 },
};

// The size of a part as placed, after its scale.
function sizeOf(part: ScenePart) {
  const scale = part.scale ?? 1;
  return { w: SIZES[part.kind].w * scale, h: SIZES[part.kind].h * scale };
}

// The space left between a part's edge and the end of a line.
const GAP = 9;

function PartShape({ kind }: { kind: PartKind }) {
  switch (kind) {
    case "you":
      return (
        <>
          <circle className="gage-how__shape" cx="34" cy="18" r="18" />
          <path className="gage-how__shape" d="M0 95q0-44 34-44q34 0 34 44" />
        </>
      );
    case "agent":
      return (
        <>
          <rect className="gage-how__shape" width="210" height="130" rx="10" />
          <path className="gage-how__shape" d="M0 24h210" />
          <circle className="gage-how__dot" cx="16" cy="12" r="3" />
          <circle className="gage-how__dot" cx="29" cy="12" r="3" />
          <circle className="gage-how__dot" cx="42" cy="12" r="3" />
          <path className="gage-how__shape" d="M18 46l8 6l-8 6" />
          <path className="gage-how__detail" d="M36 52h64M18 76h132M18 98h96" />
        </>
      );
    case "model":
      return (
        <path
          className="gage-how__shape"
          d="M24 60a24 24 0 0 1 2-47a34 34 0 0 1 64-6a27 27 0 0 1 14 53z"
        />
      );
    case "gage":
      return (
        <>
          <rect
            className="gage-how__shape gage-how__shape--gage"
            width="160"
            height="120"
            rx="10"
          />
          <rect
            className="gage-how__detail"
            x="22"
            y="20"
            width="116"
            height="54"
            rx="4"
          />
          <path
            className="gage-how__detail"
            d="M22 37h116M22 56h116M60 20v54M99 20v54"
          />
        </>
      );
    case "logs":
      return (
        <>
          <path
            className="gage-how__shape gage-how__paper"
            d="M24 0h48l22 22v66h-70z"
          />
          <path
            className="gage-how__shape gage-how__paper"
            d="M12 8h48l22 22v66h-70z"
          />
          <path
            className="gage-how__shape gage-how__paper"
            d="M0 16h48l22 22v66h-70zM48 16v22h22"
          />
          <path className="gage-how__detail" d="M12 58h46M12 72h46M12 86h30" />
        </>
      );
    case "issues":
      return (
        <>
          <rect className="gage-how__shape" width="94" height="104" rx="8" />
          <circle className="gage-how__shape" cx="20" cy="28" r="5" />
          <circle className="gage-how__shape" cx="20" cy="52" r="5" />
          <circle className="gage-how__shape" cx="20" cy="76" r="5" />
          <path className="gage-how__detail" d="M36 28h42M36 52h42M36 76h28" />
        </>
      );
  }
}

function PartLabel({ part }: { part: ScenePart }) {
  const { w, h } = sizeOf(part);
  if (part.labelAt === "right") {
    return (
      <text className="gage-how__label" x={w + 14} y={h / 2 + 6}>
        {part.label}
      </text>
    );
  }
  const y = part.labelAt === "inside" ? h - 17 : h + 27;
  return (
    <text className="gage-how__label" x={w / 2} y={y} textAnchor="middle">
      {part.label}
    </text>
  );
}

const OUT: Record<Side, [number, number]> = {
  top: [0, -1],
  right: [1, 0],
  bottom: [0, 1],
  left: [-1, 0],
};

// The point where a line ends, just clear of the part's edge.
function endPoint(parts: ScenePart[], end: End): [number, number] {
  const part = parts.find((p) => p.id === end.part);
  if (!part) return [0, 0];
  const { w, h } = sizeOf(part);
  const [ox, oy] = OUT[end.side];
  const along = end.side === "top" || end.side === "bottom";
  const x = along ? end.at * w : end.side === "right" ? w : 0;
  const y = along ? (end.side === "bottom" ? h : 0) : end.at * h;
  return [part.x + x + ox * GAP, part.y + y + oy * GAP];
}

// A curve that leaves each part square to its side. It returns the path
// and the point halfway along it, where the callout badge goes.
function linkPath(parts: ScenePart[], from: End, to: End, level?: boolean) {
  const [x0, y0] = endPoint(parts, from);
  let [x3, y3] = endPoint(parts, to);
  if (level) {
    if (from.side === "left" || from.side === "right") y3 = y0;
    else x3 = x0;
  }
  const reach = (side: Side) =>
    0.5 *
    (side === "left" || side === "right"
      ? Math.abs(x3 - x0)
      : Math.abs(y3 - y0));
  const [fx, fy] = OUT[from.side];
  const [tx, ty] = OUT[to.side];
  const x1 = x0 + fx * reach(from.side);
  const y1 = y0 + fy * reach(from.side);
  const x2 = x3 + tx * reach(to.side);
  const y2 = y3 + ty * reach(to.side);
  return {
    d: `M${x0} ${y0}C${x1} ${y1} ${x2} ${y2} ${x3} ${y3}`,
    mid: [(x0 + 3 * x1 + 3 * x2 + x3) / 8, (y0 + 3 * y1 + 3 * y2 + y3) / 8] as [
      number,
      number,
    ],
  };
}

// Whether a part is drawn in the accent color at a given frame.
function accented(view: SceneView, id: string, frame: number): boolean {
  const range = view.accent?.[id];
  return range !== undefined && frame >= range[0] && frame <= range[1];
}

// What a thing looks like at a given frame: not yet there, just
// arrived, or there from an earlier frame.
function stateAt(appears: number, frame: number): "hidden" | "new" | "old" {
  if (appears > frame) return "hidden";
  return appears === frame ? "new" : "old";
}

// A coordinate grid for arranging the drawing, shown with ?grid.
function Grid() {
  const lines = [];
  for (let x = 0; x <= SCENE.width; x += 20) {
    lines.push(
      <path
        key={`x${x}`}
        className={
          x % 100 === 0
            ? "gage-how__grid gage-how__grid--major"
            : "gage-how__grid"
        }
        d={`M${x} 0V${SCENE.height}`}
      />,
    );
    if (x % 100 === 0) {
      lines.push(
        <text key={`tx${x}`} className="gage-how__grid-text" x={x + 3} y={11}>
          {x}
        </text>,
      );
    }
  }
  for (let y = 0; y <= SCENE.height; y += 20) {
    lines.push(
      <path
        key={`y${y}`}
        className={
          y % 100 === 0
            ? "gage-how__grid gage-how__grid--major"
            : "gage-how__grid"
        }
        d={`M0 ${y}H${SCENE.width}`}
      />,
    );
    if (y % 100 === 0 && y > 0) {
      lines.push(
        <text key={`ty${y}`} className="gage-how__grid-text" x={3} y={y - 3}>
          {y}
        </text>,
      );
    }
  }
  return <g>{lines}</g>;
}

// The size of a callout's icon, and the space between it and the
// words, in drawing units.
const CALLOUT_ICON = 20;
const CALLOUT_GAP = 7;

// A note drawn as a callout: an icon and words that link to the
// evidence for what they claim. The note's y is the baseline of the
// first line.
function Callout({ note, href }: { note: SceneNote; href: string }) {
  // A link to another site opens in a new tab, like the site's other
  // outside links.
  const outside = /^https?:/.test(href);
  return (
    <a
      className="gage-how__callout"
      href={href}
      target={outside ? "_blank" : undefined}
      rel={outside ? "noopener noreferrer" : undefined}
    >
      <Zap
        x={note.x}
        y={note.y - CALLOUT_ICON + 4}
        width={CALLOUT_ICON}
        height={CALLOUT_ICON}
        strokeWidth={2}
        aria-hidden="true"
      />
      <text
        className="gage-how__note gage-how__note--callout"
        x={note.x + CALLOUT_ICON + CALLOUT_GAP}
        y={note.y}
      >
        {note.text.split("\n").map((line, index) => (
          <tspan
            key={line}
            x={note.x + CALLOUT_ICON + CALLOUT_GAP}
            dy={index === 0 ? 0 : "1.35em"}
          >
            {line}
          </tspan>
        ))}
      </text>
    </a>
  );
}

// The height of a line of caption words, in drawing units: the 16px
// note size times the 1.35 line spacing.
const CAPTION_LINE = 21.6;

// The size of a command caption's pane. A mono letter at the 15px
// command size is 9 units wide, which gives the pane its width.
const CODE_LETTER = 9;
const CODE_PAD = 9;
const CODE_HEIGHT = 28;

// A caption that is a command: one line of mono words in a bordered
// pane. The caption's anchor places the pane, not the words, so an
// "end" caption has the pane's right edge at x.
function CodeCaption({
  caption,
  mid,
}: {
  caption: Caption;
  mid: [number, number];
}) {
  const w = caption.text.length * CODE_LETTER + 2 * CODE_PAD;
  const x = mid[0] + caption.dx;
  const y = mid[1] + caption.dy;
  const left =
    caption.anchor === "start"
      ? x
      : caption.anchor === "end"
        ? x - w
        : x - w / 2;
  return (
    <>
      <rect
        className="gage-how__pane"
        x={left}
        y={y - 19}
        width={w}
        height={CODE_HEIGHT}
        rx="6"
      />
      <text
        className="gage-how__note gage-how__note--code"
        x={left + w / 2}
        y={y}
        textAnchor="middle"
      >
        {caption.text}
      </text>
    </>
  );
}

// The words on a line. A caption above its line grows upward, so the
// last line of words keeps its distance from the line.
function LinkCaption({
  caption,
  mid,
}: {
  caption: Caption;
  mid: [number, number];
}) {
  if (caption.code) return <CodeCaption caption={caption} mid={mid} />;
  const lines = caption.text.split("\n");
  const x = mid[0] + caption.dx;
  const rise = caption.dy < 0 ? (lines.length - 1) * CAPTION_LINE : 0;
  return (
    <text
      className="gage-how__note"
      x={x}
      y={mid[1] + caption.dy - rise}
      textAnchor={caption.anchor}
    >
      {lines.map((line, index) => (
        <tspan key={line} x={x} dy={index === 0 ? 0 : CAPTION_LINE}>
          {line}
        </tspan>
      ))}
    </text>
  );
}

export function HowDiagram({
  parts,
  view,
  frame,
}: {
  parts: ScenePart[];
  view: SceneView;
  frame: number;
}) {
  const [grid, setGrid] = useState(false);
  useEffect(() => {
    setGrid(new URLSearchParams(window.location.search).has("grid"));
  }, []);
  return (
    <svg
      className="gage-how__svg"
      viewBox={`0 0 ${SCENE.width} ${SCENE.height}`}
      role="img"
      aria-label={`${view.title}: a diagram of you, your harness, the model and your session logs`}
    >
      <defs>
        <marker
          id="gage-how-arrow"
          viewBox="0 0 10 10"
          refX="8.5"
          refY="5"
          markerWidth="7"
          markerHeight="7"
          orient="auto-start-reverse"
        >
          <path className="gage-how__head" d="M1 1.5 8.5 5 1 8.5" />
        </marker>
        <marker
          id="gage-how-arrow-new"
          viewBox="0 0 10 10"
          refX="8.5"
          refY="5"
          markerWidth="7"
          markerHeight="7"
          orient="auto-start-reverse"
        >
          <path
            className="gage-how__head gage-how__head--new"
            d="M1 1.5 8.5 5 1 8.5"
          />
        </marker>
      </defs>
      {grid && <Grid />}
      {view.links.map((link) => {
        const { d, mid } = linkPath(parts, link.from, link.to, link.level);
        const state =
          link.until !== undefined && frame > link.until
            ? "hidden"
            : stateAt(link.frame, frame);
        // A line that belongs to this view's story is drawn in the
        // accent color. Background lines, from frame 0, are not.
        const accent = link.frame > 0 && view.parts.gage !== undefined;
        const head = accent
          ? "url(#gage-how-arrow-new)"
          : "url(#gage-how-arrow)";
        return (
          <g
            key={`${link.from.part}-${link.from.side}-${link.to.part}-${link.frame}`}
            className="gage-how__step"
            data-state={state}
          >
            <path
              className={
                accent ? "gage-how__line gage-how__line--new" : "gage-how__line"
              }
              d={d}
              markerStart={link.both ? head : undefined}
              markerEnd={head}
            />
            {link.caption && <LinkCaption caption={link.caption} mid={mid} />}
          </g>
        );
      })}
      {parts.map((part) => {
        const appears = view.parts[part.id];
        const state =
          appears === undefined ? "hidden" : stateAt(appears, frame);
        return (
          <g
            key={part.id}
            className="gage-how__part"
            data-kind={part.kind}
            data-state={state}
            data-accent={accented(view, part.id, frame) ? "" : undefined}
            transform={`translate(${part.x} ${part.y})`}
          >
            <g transform={`scale(${part.scale ?? 1})`}>
              <PartShape kind={part.kind} />
            </g>
            <PartLabel part={part} />
          </g>
        );
      })}
      {view.notes?.map((note) => (
        <g
          key={`${note.frame}:${note.x}:${note.y}:${note.text}`}
          className="gage-how__step"
          data-state={
            note.until !== undefined && frame > note.until
              ? "hidden"
              : stateAt(note.frame, frame)
          }
          data-steady={note.steady ? "" : undefined}
        >
          {note.callout ? (
            <Callout note={note} href={note.callout.href} />
          ) : (
            <text
              className={
                note.dim
                  ? "gage-how__note gage-how__note--dim"
                  : "gage-how__note"
              }
              x={note.x}
              y={note.y}
              textAnchor={note.anchor}
            >
              {note.text.split("\n").map((line, index) => (
                <tspan key={line} x={note.x} dy={index === 0 ? 0 : "1.35em"}>
                  {line}
                </tspan>
              ))}
            </text>
          )}
        </g>
      ))}
    </svg>
  );
}
