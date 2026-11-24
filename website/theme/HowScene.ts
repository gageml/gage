// The layout of the "How it works" drawing. This file is the only place
// positions and captions are written, so the drawing is rearranged by
// editing it. HowDiagram.tsx draws each part once in its own box and
// places it at the x and y given below. Lines are drawn between named
// sides of parts, so a line follows its parts when they move.
//
// The file holds one drawing per scenario. A scenario is a setting Gage
// is used in, such as a solo developer, and the reader picks one with
// the buttons above the drawing. Each scenario has its own parts and
// views.
//
// A drawing has views, and each view is built up in frames. A part or
// a line names the frame in which it first appears. Frame 0 means it is
// there from the start of the view, as background.
//
// Open the home page with ?grid added to the address to see a
// coordinate grid over the drawing.

export type PartKind = "you" | "agent" | "model" | "gage" | "logs" | "issues";
export type Side = "top" | "right" | "bottom" | "left";

export interface ScenePart {
  id: string;
  kind: PartKind;
  x: number;
  y: number;
  label: string;
  labelAt: "below" | "right" | "inside";
  // Size relative to the part as drawn. 0.75 is three quarters.
  scale?: number;
}

// Where a line meets a part: a side, and how far along that side, from
// 0 (its top or left end) to 1 (its bottom or right end).
export interface End {
  part: string;
  side: Side;
  at: number;
}

// Words written on the drawing. For a line they are placed from the
// middle of the line by dx and dy. For a note they are placed at x, y.
// A line break in the text starts a new line. A caption above its line
// (dy below 0) grows upward, so its last line of words stays at dy.
export interface Caption {
  text: string;
  dx: number;
  dy: number;
  anchor: "start" | "middle" | "end";
  // The words are a command the reader types, drawn on one line in a
  // mono font inside a bordered pane. The anchor then places the pane.
  code?: boolean;
}

export interface SceneLink {
  from: End;
  to: End;
  frame: number;
  // The last frame in which the line is shown. Left out, it stays.
  until?: number;
  // Draw the line straight and level (or plumb), taking its height (or
  // its left position) from the end it starts at.
  level?: boolean;
  // Arrowheads at both ends.
  both?: boolean;
  caption?: Caption;
}

// Words placed on the drawing by position. A line break in the text
// starts a new line.
export interface SceneNote {
  frame: number;
  // The last frame in which the note is shown. Left out, it stays.
  until?: number;
  text: string;
  // Drawn fainter than the other captions.
  dim?: boolean;
  // Drawn at full strength on every slide it appears on, without
  // fading back as later steps arrive.
  steady?: boolean;
  x: number;
  y: number;
  anchor: "start" | "middle" | "end";
  // Draw the note as a callout: words in a color of their own with an
  // icon in front of the first line, which link to href. It makes a claim the
  // reader can follow up. A callout is placed by the left end of its
  // icon, and its anchor is not used.
  callout?: { href: string };
}

export interface SceneView {
  title: string;
  frames: number;
  // The title shown above the drawing on each frame, first frame first.
  frameTitles: string[];
  // The frame in which each part appears. A part not listed is absent.
  parts: Record<string, number>;
  // The frames in which a part is drawn in the accent color, as a
  // first and a last frame. Outside them it is drawn like the others.
  accent?: Record<string, [number, number]>;
  links: SceneLink[];
  notes?: SceneNote[];
}

export interface Scenario {
  // The text of the button that picks this scenario.
  title: string;
  parts: ScenePart[];
  views: SceneView[];
}

const HISTORY: Scenario = {
  title: "Query",
  parts: [
    { id: "you", kind: "you", x: 91, y: 92, label: "You", labelAt: "below" },
    {
      id: "agent",
      kind: "agent",
      x: 355,
      y: 92,
      label: "Harness",
      labelAt: "below",
      scale: 0.75,
    },
    {
      id: "model",
      kind: "model",
      x: 709,
      y: 110,
      label: "Model",
      labelAt: "below",
    },
    {
      id: "gage",
      kind: "gage",
      x: 245,
      y: 300,
      label: "Gage",
      labelAt: "inside",
      scale: 0.8,
    },
    {
      id: "logs",
      kind: "logs",
      x: 540,
      y: 292,
      label: "Sessions",
      labelAt: "below",
    },
  ],
  views: [
    {
      title: "With Gage",
      frames: 3,
      frameTitles: [
        "Install Gage",
        "Gage serves session history over SQL",
        "Model has fast, consistent access to full session record",
      ],
      parts: { you: 0, agent: 0, model: 0, logs: 0, gage: 1 },
      accent: { gage: [2, 2], logs: [2, 3] },
      links: [
        // Lines that are there from the start, as background.
        {
          from: { part: "agent", side: "right", at: 0.5 },
          to: { part: "model", side: "left", at: 0.5 },
          frame: 0,
          until: 1,
          both: true,
          caption: { text: "API", dx: 0, dy: -14, anchor: "middle" },
        },
        {
          from: { part: "agent", side: "bottom", at: 0.9 },
          to: { part: "logs", side: "top", at: 0.5 },
          frame: 0,
          caption: { text: "Log", dx: 18, dy: 4, anchor: "start" },
        },
        // The question takes the place of the chat line.
        {
          from: { part: "you", side: "right", at: 0.5 },
          to: { part: "agent", side: "left", at: 0.5 },
          frame: 1,
          until: 2,
          caption: {
            text: "Ask “Why this\ndesign?”",
            dx: 0,
            dy: -14,
            anchor: "middle",
          },
        },
        {
          from: { part: "agent", side: "bottom", at: 0.15 },
          to: { part: "gage", side: "top", at: 0.5 },
          frame: 2,
          until: 2,
          both: true,
          caption: { text: "MCP", dx: -16, dy: 6, anchor: "end" },
        },
        // The tool call comes from the model, in place of the API line.
        {
          from: { part: "model", side: "left", at: 0.5 },
          to: { part: "agent", side: "right", at: 0.5 },
          frame: 2,
          until: 2,
          both: true,
          caption: {
            text: "Query tool\n(SQL)",
            dx: 0,
            dy: -14,
            anchor: "middle",
          },
        },
        {
          from: { part: "gage", side: "right", at: 0.5 },
          to: { part: "logs", side: "left", at: 0.5 },
          level: true,
          both: true,
          caption: { text: "Read & index", dx: 0, dy: -14, anchor: "middle" },
          frame: 2,
          until: 2,
        },
        // The answer comes back from the model and on to you.
        {
          from: { part: "model", side: "left", at: 0.5 },
          to: { part: "agent", side: "right", at: 0.5 },
          frame: 3,
          caption: {
            text: "Answer with\ncitations",
            dx: 0,
            dy: -14,
            anchor: "middle",
          },
        },
        {
          from: { part: "agent", side: "left", at: 0.5 },
          to: { part: "you", side: "right", at: 0.5 },
          frame: 3,
          caption: {
            text: "“Here’s why…”",
            dx: 0,
            dy: -14,
            anchor: "middle",
          },
        },
      ],
      notes: [
        {
          frame: 1,
          text: "Installed & waiting",
          dim: true,
          until: 1,
          x: 309,
          y: 288,
          anchor: "middle",
        },
        // Gage is waiting again once the answer is back.
        {
          frame: 3,
          text: "Installed & waiting",
          dim: true,
          x: 309,
          y: 288,
          anchor: "middle",
        },
        // What the stack of sessions holds, written beside it.
        {
          frame: 0,
          text: "History of code edits\nand design discussions",
          steady: true,
          x: 650,
          y: 340,
          anchor: "start",
        },
        // The figures are preliminary results from the study the
        // callout links to.
        {
          frame: 2,
          until: 2,
          text: "1.5x faster + 2.4x fewer\ntokens than ripgrep",
          steady: true,
          x: 208,
          y: 428,
          anchor: "start",
          callout: {
            href: "https://github.com/gageml/gage-studies/tree/main/query-vs-rg",
          },
        },
      ],
    },
  ],
};

// What a scan is, in three slides: it reads your sessions, it reports
// the problems it finds as issues, and your agent helps you fix them.
// You, the harness, Gage and the sessions are where the Query drawing
// has them, so they stay put when the reader changes scenario.
const SCAN: Scenario = {
  title: "Scan",
  parts: [
    { id: "you", kind: "you", x: 214, y: 92, label: "You", labelAt: "below" },
    {
      id: "agent",
      kind: "agent",
      x: 478,
      y: 92,
      label: "Harness",
      labelAt: "below",
      scale: 0.75,
    },
    {
      id: "gage",
      kind: "gage",
      x: 368,
      y: 300,
      label: "Gage",
      labelAt: "inside",
      scale: 0.8,
    },
    {
      id: "logs",
      kind: "logs",
      x: 663,
      y: 292,
      label: "Sessions",
      labelAt: "below",
    },
    {
      id: "issues",
      kind: "issues",
      x: 183,
      y: 296,
      label: "Issues",
      labelAt: "below",
    },
  ],
  views: [
    {
      title: "Scan",
      frames: 3,
      frameTitles: [
        "Gage analyzes your sessions",
        "It reports issues",
        "Your agent helps you fix them",
      ],
      parts: { you: 0, agent: 0, logs: 0, gage: 1, issues: 2 },
      accent: { gage: [1, 2] },
      links: [
        // The line that is there from the start, as background.
        {
          from: { part: "agent", side: "bottom", at: 0.9 },
          to: { part: "logs", side: "top", at: 0.5 },
          frame: 0,
          caption: { text: "Log", dx: 18, dy: 4, anchor: "start" },
        },
        // You start the scan, and Gage reads the sessions.
        {
          from: { part: "you", side: "right", at: 0.85 },
          to: { part: "gage", side: "top", at: 0.2 },
          frame: 1,
          until: 2,
          caption: {
            text: "gage scan",
            dx: 2,
            dy: 36,
            anchor: "end",
            code: true,
          },
        },
        {
          from: { part: "gage", side: "right", at: 0.5 },
          to: { part: "logs", side: "left", at: 0.5 },
          frame: 1,
          until: 2,
          level: true,
          both: true,
          caption: { text: "Read", dx: 0, dy: -14, anchor: "middle" },
        },
        // The problems come out as issues.
        {
          from: { part: "gage", side: "left", at: 0.5 },
          to: { part: "issues", side: "right", at: 0.5 },
          frame: 2,
          until: 2,
          level: true,
          caption: { text: "Report", dx: 0, dy: -14, anchor: "middle" },
        },
        // On the last slide the issues go back into Gage, to be
        // resolved.
        {
          from: { part: "issues", side: "right", at: 0.5 },
          to: { part: "gage", side: "left", at: 0.5 },
          frame: 3,
          level: true,
        },
        // Gage has the harness judge what it read. The resolve line
        // takes this line's place on the next slide.
        {
          from: { part: "gage", side: "top", at: 0.5 },
          to: { part: "agent", side: "bottom", at: 0.15 },
          frame: 1,
          until: 2,
          both: true,
          caption: { text: "Judge", dx: 16, dy: 14, anchor: "start" },
        },
        // Gage reports the issues to your agent, which brings the fixes
        // to you.
        {
          from: { part: "gage", side: "top", at: 0.5 },
          to: { part: "agent", side: "bottom", at: 0.15 },
          frame: 3,
          caption: { text: "Report issues", dx: 16, dy: 14, anchor: "start" },
        },
        // You start the resolve the way you started the scan.
        {
          from: { part: "you", side: "right", at: 0.85 },
          to: { part: "gage", side: "top", at: 0.2 },
          frame: 3,
          caption: {
            text: "gage resolve",
            dx: 2,
            dy: 36,
            anchor: "end",
            code: true,
          },
        },
        {
          from: { part: "agent", side: "left", at: 0.5 },
          to: { part: "you", side: "right", at: 0.5 },
          frame: 3,
          level: true,
          both: true,
          caption: {
            text: "Resolve issues",
            dx: 0,
            dy: -14,
            anchor: "middle",
          },
        },
      ],
    },
  ],
};

export const SCENE = {
  width: 940,
  height: 470,
  scenarios: [HISTORY, SCAN] as Scenario[],
};
