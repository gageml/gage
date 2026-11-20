#!/usr/bin/env node
// Writes the Pi reference sessions under gage-pi/fixtures/pi-<version>/.
//
// Every entry is written by the installed Pi's own code: the agent runs
// in process against Pi's faux model provider, so nothing reaches the
// network and no model is called. Run it by hand (`just pi-fixtures`)
// and commit the output. `cargo test` never runs it.
//
// Usage: node generate.mjs [--pi <path to the pi executable>]
//
// The clock, the random sources, and every path Pi sees are fixed, so
// two runs against one Pi release write the same bytes.

import { spawnSync } from "node:child_process";
import nodeCrypto from "node:crypto";
import fs from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

// Pi records these paths in its sessions, so they are the same on every
// machine. PI_LINK stands in for the directory Pi is installed in.
const WORK = "/tmp/gage-pi-fixtures";
const PI_LINK = path.join(WORK, "pi");
const AGENT_DIR = path.join(WORK, "agent");
const PROJECT = path.join(WORK, "project");
const OTHER_PROJECT = path.join(WORK, "other-project");

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PACKAGE = "node_modules/@earendil-works/pi-coding-agent";

if (process.env.GAGE_PI_FIXTURES_CHILD === "1") {
  await generate();
} else {
  launch();
}

// Node resolves a module to its real path unless told otherwise, and Pi
// writes its own location into the system prompt. The generator runs
// again with symlinks preserved so that location is PI_LINK.
function launch() {
  const release = piReleaseDir(optionValue("--pi") ?? "pi");
  fs.rmSync(WORK, { recursive: true, force: true });
  fs.mkdirSync(WORK, { recursive: true });
  fs.symlinkSync(release, PI_LINK);
  const child = spawnSync(
    process.execPath,
    ["--preserve-symlinks", "--preserve-symlinks-main", fileURLToPath(import.meta.url)],
    {
      stdio: "inherit",
      env: {
        PATH: process.env.PATH,
        HOME: WORK,
        TZ: "UTC",
        PI_CODING_AGENT_DIR: AGENT_DIR,
        PI_OFFLINE: "1",
        PI_TELEMETRY: "0",
        PI_SKIP_VERSION_CHECK: "1",
        GAGE_PI_FIXTURES_CHILD: "1",
      },
    },
  );
  if (child.status !== 0) process.exit(child.status ?? 1);
  const version = JSON.parse(
    fs.readFileSync(path.join(release, PACKAGE, "package.json"), "utf8"),
  ).version;
  const out = path.join(HERE, `pi-${version}`);
  fs.rmSync(out, { recursive: true, force: true });
  fs.mkdirSync(out, { recursive: true });
  fs.cpSync(path.join(AGENT_DIR, "sessions"), path.join(out, "sessions"), { recursive: true });
  fs.copyFileSync(path.join(WORK, "stats.json"), path.join(out, "stats.json"));
  fs.rmSync(WORK, { recursive: true, force: true });
  console.log(`wrote ${path.relative(process.cwd(), out)}`);
}

function optionValue(name) {
  const index = process.argv.indexOf(name);
  return index < 0 ? undefined : process.argv[index + 1];
}

// The directory of the Pi release behind the `pi` executable. A managed
// install is `<agent dir>/bin/pi` beside `install/releases/<version>`.
function piReleaseDir(pi) {
  const found = pi.includes("/") ? pi : which(pi);
  const agentDir = path.dirname(path.dirname(fs.realpathSync(found)));
  const install = path.join(agentDir, "install");
  const version = fs.readFileSync(path.join(install, "current-version"), "utf8").trim();
  const release = path.join(install, "releases", version);
  if (!fs.existsSync(path.join(release, PACKAGE, "package.json"))) {
    throw new Error(`no Pi release at ${release}`);
  }
  return release;
}

function which(name) {
  for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
    const candidate = path.join(dir, name);
    if (fs.existsSync(candidate)) return candidate;
  }
  throw new Error(`${name} is not on PATH; pass --pi <path>`);
}

async function generate() {
  fixClockAndRandomness();
  const pi = await import(pathToFileURL(path.join(PI_LINK, PACKAGE, "dist/index.js")).href);
  const ai = await import(
    pathToFileURL(path.join(PI_LINK, "node_modules/@earendil-works/pi-ai/dist/index.js")).href
  );
  const ctx = { pi, ai, stats: {} };

  fs.mkdirSync(PROJECT, { recursive: true });
  fs.mkdirSync(OTHER_PROJECT, { recursive: true });
  fs.writeFileSync(path.join(PROJECT, "notes.txt"), "alpha\nbeta\ngamma\n");

  const runFile = await agentRun(ctx);
  await branches(ctx);
  await fork(ctx, runFile);

  fs.writeFileSync(path.join(WORK, "stats.json"), `${JSON.stringify(ctx.stats, null, 2)}\n`);
}

// An ordinary run: tool calls, a renamed session, a thinking level
// change, a changed tool set, a shell command the user ran, an injected
// message, a usage entry, and a compaction.
async function agentRun(ctx) {
  const { ai } = ctx;
  const { session, faux } = await openSession(ctx, ctx.pi.SessionManager.create(PROJECT));
  const say = ai.fauxAssistantMessage;

  faux.setResponses([
    say(
      [
        ai.fauxThinking("The notes file is the place to start."),
        ai.fauxToolCall("read", { path: "notes.txt" }),
      ],
      { stopReason: "toolUse" },
    ),
    say([ai.fauxToolCall("bash", { command: "wc -l notes.txt" })], { stopReason: "toolUse" }),
    say("notes.txt has three lines: alpha, beta, and gamma."),
  ]);
  await session.prompt("What is in notes.txt?");

  session.setSessionName("Reading the notes");
  session.setThinkingLevel("high");
  session.setActiveToolsByName(["read"]);
  await session.executeBash("echo delta >> notes.txt");
  await session.sendCustomMessage({
    customType: "gage-fixture",
    content: "The user prefers short answers.",
    display: true,
    details: { source: "generate.mjs" },
  });

  faux.setResponses([
    say([ai.fauxToolCall("read", { path: "missing.txt" })], { stopReason: "toolUse" }),
    say("There is no missing.txt. notes.txt now ends with delta."),
  ]);
  await session.prompt("Now read missing.txt.");

  // Pi's cache warmer writes this entry after a request that only
  // refreshes the provider's prompt cache. The warmer runs on a timer,
  // so the entry is written here through the same call it makes.
  const model = faux.getModel();
  const usage = (input, cacheRead) => {
    const counts = {
      input,
      output: 0,
      cacheRead,
      cacheWrite: 0,
      totalTokens: input + cacheRead,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
    };
    ai.calculateCost(model, counts);
    return counts;
  };
  const manager = session.sessionManager;
  manager.appendUsage("cache_warm", model.provider, model.id, usage(0, 1840));
  // A kind Pi does not define, which a reader has to accept, with
  // costs small enough that JavaScript writes one of them as 5e-7.
  manager.appendUsage("gage_fixture", model.provider, model.id, usage(1, 1), "one token each");

  // The cut falls inside the last turn, so Pi asks for two summaries:
  // one of the history and one of the turn's kept-out prefix.
  faux.setResponses([
    say("## Goal\nRead the project notes.\n\n## Progress\nnotes.txt was read and extended."),
    say("The user asked for missing.txt, which does not exist."),
  ]);
  await session.compact();
  if (faux.getPendingResponseCount() !== 0) throw new Error("compaction left a response unused");

  faux.setResponses([say("Yes: alpha, beta, gamma, delta.")]);
  await session.prompt("Can you list the lines once more?");

  return finish(ctx, "run", session);
}

// A session with two branches, a branch summary, labels, context
// edits, and an extension's state entry.
async function branches(ctx) {
  const { ai } = ctx;
  const { session, faux } = await openSession(ctx, ctx.pi.SessionManager.create(PROJECT));
  const say = ai.fauxAssistantMessage;
  const manager = session.sessionManager;

  faux.setResponses([say("Sorting by name is the simplest approach.")]);
  await session.prompt("How should the notes be ordered?");
  const firstAnswer = manager.getLeafId();

  faux.setResponses([
    say([ai.fauxToolCall("bash", { command: "sort notes.txt" })], { stopReason: "toolUse" }),
    say("Sorted by name."),
  ]);
  await session.prompt("Do that.");
  const abandonedLeaf = manager.getLeafId();
  const toolResult = manager
    .getEntries()
    .find((entry) => entry.type === "message" && entry.message.role === "toolResult");

  manager.appendLabelChange(firstAnswer, "before-sorting");
  manager.appendCustomEntry("gage-fixture-state", { step: 2, done: false });
  manager.appendContextEdit(toolResult.id, { content: "(sorted output omitted)" });

  faux.setResponses([say("The user sorted the notes by name and then went back.")]);
  await session.navigateTree(firstAnswer, { summarize: true, label: "by-name" });

  faux.setResponses([say("Ordering by date keeps the newest note last.")]);
  await session.prompt("Order them by date instead.");
  const dateQuestion = manager
    .getBranch()
    .findLast((entry) => entry.type === "message" && entry.message.role === "user");
  manager.appendContextEdit(dateQuestion.id, null);
  manager.appendLabelChange(firstAnswer, undefined);

  if (!manager.getEntry(abandonedLeaf)) throw new Error("the abandoned branch was not kept");
  return finish(ctx, "branches", session);
}

// A fork of the first session into another project: a new header that
// names its parent, over the parent's entries.
async function fork(ctx, parentFile) {
  const manager = ctx.pi.SessionManager.forkFrom(parentFile, OTHER_PROJECT);
  const { session, faux } = await openSession(ctx, manager, OTHER_PROJECT);
  faux.setResponses([ctx.ai.fauxAssistantMessage("This project has no notes yet.")]);
  await session.prompt("What about this project?");
  return finish(ctx, "fork", session);
}

async function openSession(ctx, sessionManager, cwd = PROJECT) {
  const { pi, ai } = ctx;
  const core = ai.createFauxCore({
    api: "faux",
    provider: "faux",
    models: [
      {
        id: "faux-1",
        name: "Faux 1",
        reasoning: true,
        cost: { input: 5, output: 25, cacheRead: 0.5, cacheWrite: 6.25 },
      },
    ],
  });
  const provider = ai.createProvider({
    id: core.provider,
    auth: { apiKey: { name: "Faux", resolve: async () => ({ auth: {} }) } },
    models: core.models,
    api: {
      stream: priced(ai, core.stream),
      streamSimple: priced(ai, core.streamSimple),
      fetchDeferred: core.fetchDeferred,
      cancelDeferred: core.cancelDeferred,
    },
  });
  const modelRuntime = await pi.ModelRuntime.create({
    authPath: path.join(AGENT_DIR, "auth.json"),
    modelsPath: path.join(AGENT_DIR, "models.json"),
  });
  modelRuntime.registerNativeProvider(provider);
  const { session } = await pi.createAgentSession({
    cwd,
    agentDir: AGENT_DIR,
    model: core.getModel(),
    thinkingLevel: "low",
    modelRuntime,
    tools: ["read", "bash"],
    sessionManager,
    // A small keepRecentTokens lets the manual compaction of a short
    // session find something to summarize.
    settingsManager: pi.SettingsManager.inMemory({
      compaction: { enabled: false, keepRecentTokens: 40 },
    }),
    resourceLoader: {
      getExtensions: () => ({ extensions: [], errors: [], runtime: pi.createExtensionRuntime() }),
      getSkills: () => ({ skills: [], diagnostics: [] }),
      getPrompts: () => ({ prompts: [], diagnostics: [] }),
      getThemes: () => ({ themes: [], diagnostics: [] }),
      getAgentsFiles: () => ({ agentsFiles: [] }),
      getSystemPrompt: () => undefined,
      getSystemPromptSource: () => undefined,
      getAppendSystemPrompt: () => [],
      getAppendSystemPromptSources: () => [],
      extendResources: () => {},
      reload: async () => {},
    },
  });
  return { session, faux: core };
}

// The faux provider estimates token counts and leaves every cost at
// zero. A real provider prices its response with Pi's `calculateCost`;
// this does the same to the faux provider's finished message.
function priced(ai, stream) {
  return (model, context, options) => {
    const events = stream(model, context, options);
    const push = events.push.bind(events);
    events.push = (event) => {
      if (event.type === "done") ai.calculateCost(model, event.message.usage);
      push(event);
    };
    return events;
  };
}

// Record Pi's own totals for the session beside the fixtures, so a test
// can hold the driver to the numbers Pi reports.
async function finish(ctx, name, session) {
  await session.waitForIdle();
  const file = session.sessionFile;
  const stats = session.getSessionStats();
  ctx.stats[name] = {
    file: path.relative(path.join(AGENT_DIR, "sessions"), file),
    sessionId: stats.sessionId,
    entries: session.sessionManager.getEntryCount(),
    userMessages: stats.userMessages,
    assistantMessages: stats.assistantMessages,
    toolCalls: stats.toolCalls,
    toolResults: stats.toolResults,
    tokens: stats.tokens,
    cost: stats.cost,
  };
  session.dispose();
  return file;
}

// Pi stamps entries from the clock and names them from random bytes.
// Both are replaced before Pi loads: each reading of the clock is 250 ms
// after the last, and the random sources are seeded.
function fixClockAndRandomness() {
  const RealDate = Date;
  let now = RealDate.UTC(2026, 10, 20, 12, 0, 0);
  const tick = () => {
    now += 250;
    return now;
  };
  const start = now;
  globalThis.Date = class extends RealDate {
    constructor(...args) {
      if (args.length === 0) super(tick());
      else super(...args);
    }
    static now() {
      return tick();
    }
  };
  performance.now = () => tick() - start;

  let seed = 0x67616765;
  const random = () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const fill = (bytes) => {
    for (let i = 0; i < bytes.length; i++) bytes[i] = Math.floor(random() * 256);
    return bytes;
  };
  const uuid = () => {
    const b = fill(new Uint8Array(16));
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    const hex = Array.from(b, (byte) => byte.toString(16).padStart(2, "0")).join("");
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  };
  Math.random = random;
  nodeCrypto.randomUUID = uuid;
  syncBuiltinESMExports();
  globalThis.crypto.randomUUID = uuid;
  globalThis.crypto.getRandomValues = fill;
}
