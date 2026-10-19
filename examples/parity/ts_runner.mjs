#!/usr/bin/env node
/**
 * TypeScript side of the PD memory parity harness.
 *
 * Runs ONE deterministic scenario (see scenarios.json) against PilotDeck's
 * compiled TS memory core and writes a normalized artifact JSON that
 * `compare.py` diffs against the Rust artifact produced by `rust_runner.py`.
 *
 * Usage:
 *   node ts_runner.mjs --scenario P1 --run-dir <dir> --port <mock-port>
 *
 * Everything the TS side consumes is plain files/Javascript already present in
 * this checkout; no npm install and no network are required.
 */

import { mkdirSync, readdirSync, readFileSync, statSync, writeFileSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

// `node:sqlite` is still flagged experimental on node >=22; that warning is
// harmless and is ignored by the harness.
let DatabaseSync = null;
try {
  ({ DatabaseSync } = await import("node:sqlite"));
} catch {
  DatabaseSync = null;
}

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = process.env.PILOTDECK_ROOT ?? resolve(here, "../../../..");

function parseArgs(argv) {
  const options = {
    scenario: null,
    scenarios: join(here, "scenarios.json"),
    runDir: null,
    port: null,
    out: null,
    repoRoot,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      i += 1;
      if (i >= argv.length) throw new Error(`missing value for ${arg}`);
      return argv[i];
    };
    switch (arg) {
      case "--scenario":
        options.scenario = next();
        break;
      case "--scenarios":
        options.scenarios = next();
        break;
      case "--run-dir":
        options.runDir = next();
        break;
      case "--port":
        options.port = Number(next());
        break;
      case "--out":
        options.out = next();
        break;
      case "--repo-root":
        options.repoRoot = next();
        break;
      case "--help":
      case "-h":
        process.stdout.write(
          "usage: node ts_runner.mjs --scenario <name> --run-dir <dir> --port <port> [--out file]\n"
        );
        process.exit(0);
        break;
      default:
        throw new Error(`unknown argument ${arg}`);
    }
  }
  if (!options.scenario) throw new Error("--scenario is required");
  if (!options.runDir) throw new Error("--run-dir is required");
  if (!options.port) throw new Error("--port is required");
  options.runDir = resolve(options.runDir);
  if (!options.out) options.out = join(options.runDir, "ts", "artifact.json");
  return options;
}

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function walk(dir, base = dir, out = []) {
  let names = [];
  try {
    names = readdirSync(dir).sort();
  } catch {
    return out;
  }
  for (const name of names) {
    if (["control.sqlite", "control.sqlite-wal", "control.sqlite-shm"].includes(name)) continue;
    const path = join(dir, name);
    if (statSync(path).isDirectory()) walk(path, base, out);
    else out.push({ path: path.slice(base.length + 1), content: readFileSync(path, "utf8") });
  }
  return out;
}

function readL0(dbPath) {
  if (!DatabaseSync) return [];
  let db = null;
  try {
    db = new DatabaseSync(dbPath, { readOnly: true });
    const rows = db
      .prepare("SELECT * FROM l0_sessions ORDER BY timestamp ASC, created_at ASC")
      .all();
    return rows.map((row) => ({
      l0IndexId: String(row.l0_index_id),
      sessionKey: String(row.session_key),
      timestamp: String(row.timestamp),
      messages: JSON.parse(String(row.messages_json ?? "[]")),
      source: String(row.source ?? ""),
      indexed: Boolean(row.indexed),
      createdAt: String(row.created_at),
    }));
  } catch (error) {
    return [{ error: String(error) }];
  } finally {
    try {
      db?.close();
    } catch {
      /* ignore */
    }
  }
}

function pickListEntry(entry) {
  return {
    name: entry.name,
    description: entry.description,
    type: entry.type,
    scope: entry.scope,
    projectId: entry.projectId,
    capturedAt: entry.capturedAt,
    updatedAt: entry.updatedAt,
    sourceSessionKey: entry.sourceSessionKey,
    relativePath: entry.relativePath,
    deprecated: Boolean(entry.deprecated),
    dreamAttempts: entry.dreamAttempts ?? 0,
  };
}

function pickRetrieval(result) {
  const debug = result.debug ?? {};
  return {
    query: result.query,
    intent: result.intent,
    context: result.context,
    systemContext: result.systemContext ?? "",
    selectedFileIds: debug.selectedFileIds ?? [],
    debug: {
      mode: debug.mode ?? null,
      path: debug.path ?? null,
      route: debug.route ?? null,
      manifestCount: debug.manifestCount ?? null,
      preflightReason: debug.preflightReason ?? null,
    },
  };
}

function pickStats(stats) {
  return {
    capturedSessions: stats.capturedSessions,
    writtenFiles: stats.writtenFiles,
    writtenUserFiles: stats.writtenUserFiles,
    writtenProjectFiles: stats.writtenProjectFiles,
    writtenFeedbackFiles: stats.writtenFeedbackFiles,
    userProfilesUpdated: stats.userProfilesUpdated,
    failedSessions: stats.failedSessions,
  };
}

function pickOverview(overview) {
  return {
    pendingSessions: overview.pendingSessions,
    workspaceMode: overview.workspaceMode,
    currentProjectCount: overview.currentProjectCount,
    projectMetaPresent: overview.projectMetaPresent,
    projectMemoryCount: overview.projectMemoryCount,
    feedbackMemoryCount: overview.feedbackMemoryCount,
    userProfileCount: overview.userProfileCount,
    lastIndexedAt: overview.lastIndexedAt ?? null,
    recentIndexTraceCount: overview.recentIndexTraceCount,
    recentRecallTraceCount: overview.recentRecallTraceCount,
  };
}

function pickDream(result) {
  return {
    reviewedFiles: result.reviewedFiles,
    rewrittenProjects: result.rewrittenProjects,
    deletedProjects: result.deletedProjects,
    deletedFiles: result.deletedFiles,
    profileUpdated: result.profileUpdated,
    duplicateTopicCount: result.duplicateTopicCount,
    conflictTopicCount: result.conflictTopicCount,
    summary: result.summary,
    trigger: result.trigger,
    status: result.status,
  };
}

async function runtimeProbe(service, step, buildContext) {
  const RealDate = globalThis.Date;
  let now = RealDate.parse(step.now);
  globalThis.Date = class extends RealDate {
    constructor(...args) { super(...(args.length ? args : [now])); }
    static now() { return now; }
  };
  const originalFlush = service.flush;
  let flushReason = null;
  service.flush = async function(options = {}) {
    flushReason = options.reason ?? null;
    return originalFlush.call(this, options);
  };
  const events = [];
  try {
    for (const action of step.actions) {
      let event = {};
      switch (action.op) {
        case "capture":
          event.captured = service.captureTurn(action.messages, {
            sessionKey: action.session ?? "runtime", timestamp: action.timestamp ?? new Date().toISOString(), source: "parity",
          }).captured;
          break;
        case "flush": {
          const stats = await service.flush({ reason: action.reason ?? "manual", batchSize: action.batchSize, sessionKeys: action.sessionKeys });
          event = { written: stats.writtenFiles, processed: stats.capturedSessions, failed: stats.failedSessions };
          break;
        }
        case "maintain": {
          const result = await service.runDueScheduledMaintenance(action.reason ?? "scheduled");
          event = { indexRan: result.indexRan, dreamRan: result.dreamRan, indexReason: result.indexRan ? flushReason : null };
          break;
        }
        case "recall": {
          const result = action.explicit
            ? await service.search(action.query, { recentMessages: action.recent ?? [] })
            : await service.retrieveContext(action.query, { recentMessages: action.recent ?? [] });
          event = { cacheHit: result.debug?.cacheHit ?? false, route: result.debug?.route ?? result.intent,
            context: buildContext(result.context ?? "") ?? "", selectedCount: result.debug?.selectedFileIds?.length ?? 0 };
          break;
        }
        case "advance": now += action.ms; break;
        case "state":
          if (action.value == null) service.repository.deletePipelineState(action.key);
          else service.repository.setPipelineState(action.key, action.key === "indexingSettings"
            ? Object.fromEntries(Object.entries(action.value).map(([key, value]) => [key.replace(/_([a-z])/g, (_, letter) => letter.toUpperCase()), value]))
            : action.value);
          break;
        case "settings": {
          const settings = service.saveSettings(Object.fromEntries(Object.entries(action.values).map(([key, value]) =>
            [key.replace(/_([a-z])/g, (_, letter) => letter.toUpperCase()), value])));
          event = { indexMinutes: settings.autoIndexIntervalMinutes, dreamMinutes: settings.autoDreamIntervalMinutes };
          break;
        }
        case "clear": service.clear(action.scope ?? "current_project"); break;
        case "import": {
          const bundle = service.exportBundle();
          for (const key of ["lastIndexedAt", "lastDreamAt", "lastDreamStatus", "lastDreamSummary", "recentCaseTraces", "recentIndexTraces", "recentDreamTraces"]) delete bundle[key];
          service.importBundle(bundle);
          break;
        }
        case "edit_body": {
          const entry = service.list({}).find(entry => entry.type === "project");
          const content = readFileSync(entry.absolutePath, "utf8");
          const edited = content.split(action.from).join(action.to);
          if (content === edited) throw new Error("edit must change the fixture body");
          writeFileSync(entry.absolutePath, edited);
          break;
        }
        default: throw new Error(`unknown runtime op ${action.op}`);
      }
      event.op = action.op;
      event.pending = service.repository.countPendingDialogueTurns();
      event.fileCount = service.list({}).length;
      for (const key of ["autoIndexAnchorAt", "autoDreamAnchorAt", "lastIndexedAt"]) {
        event[key] = service.repository.getPipelineState(key) ?? null;
      }
      events.push(event);
    }
    return { events };
  } finally {
    globalThis.Date = RealDate;
    service.flush = originalFlush;
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const scenarioFile = readJson(options.scenarios);
  const spec = scenarioFile.scenarios[options.scenario];
  if (!spec) throw new Error(`unknown scenario ${options.scenario} in ${options.scenarios}`);

  const tsRoot = join(options.runDir, "ts", "root");
  rmSync(join(options.runDir, "ts"), { recursive: true, force: true });
  mkdirSync(tsRoot, { recursive: true });

  const modulePath = join(
    options.repoRoot,
    "src/context/memory/edgeclaw-memory-core/lib/index.js"
  );
  const moduleUrl = pathToFileURL(modulePath).href;
  const { EdgeClawMemoryService, buildMemoryRecallSystemContext } = await import(moduleUrl);

  const llm = {
    provider: "mock",
    model: "mock-model",
    baseUrl: `http://127.0.0.1:${options.port}/v1`,
    apiKey: "sk-mock",
    apiType: "openai-completions",
  };
  const services = new Map();
  const workspaces = {};

  for (const [alias, workspaceSpec] of Object.entries(spec.workspaces)) {
    const workspaceDir = resolve(options.runDir, workspaceSpec.path);
    mkdirSync(workspaceDir, { recursive: true });
    const service = new EdgeClawMemoryService({
      workspaceDir,
      rootDir: tsRoot,
      captureStrategy: "last_turn",
      includeAssistant: true,
      maxMessageChars: 6000,
      heartbeatBatchSize: 30,
      llm,
    });
    services.set(alias, service);
    workspaces[alias] = {
      alias,
      workspacePath: workspaceDir,
      hash: service.dataDir.split("/").pop(),
      dataDir: service.dataDir,
      memoryDir: service.memoryDir,
      results: {},
      files: [],
      l0: [],
    };
  }

  const failures = [];
  const runResult = async (alias, step, action) => {
    const entry = workspaces[alias];
    const requestLog = join(options.runDir, "mock/requests.jsonl");
    const readRequests = () => {
      try { return readFileSync(requestLog, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse); }
      catch (error) { if (error.code === "ENOENT") return []; throw error; }
    };
    const offset = readRequests().length;
    try {
      entry.results[step.id] = { op: step.op, ok: true, ...(await action()) };
    } catch (error) {
      entry.results[step.id] = { op: step.op, ok: false, error: String(error) };
      if (step.op !== "rollback") failures.push(`${step.id}: ${String(error)}`);
    }
    const service = services.get(alias);
    const result = entry.results[step.id];
    result.runtimeState = Object.fromEntries(
      ["lastDreamAt", "lastDreamStatus", "lastDreamSummary"]
        .map(key => [key, service.repository.getPipelineState(key)])
        .filter(([, value]) => value != null)
    );
    try { result.dreamSnapshot = readJson(join(entry.dataDir, "last_dream/metadata.json")); }
    catch (error) { if (error.code === "ENOENT") result.dreamSnapshot = null; else throw error; }
    result.modelRequests = readRequests().slice(offset);
  };

  for (const step of spec.steps) {
    const alias = step.ws;
    const service = services.get(alias);
    if (!service) throw new Error(`step ${step.id} references unknown workspace ${alias}`);
    switch (step.op) {
      case "runtime":
        await runResult(alias, step, () => runtimeProbe(service, step, buildMemoryRecallSystemContext));
        break;
      case "capture":
        await runResult(alias, step, async () => {
          const result = service.captureTurn(step.messages, {
            sessionKey: step.session,
            timestamp: step.timestamp,
            source: step.source ?? "parity",
          });
          return {
            captured: result.captured,
            sessionKey: result.sessionKey,
            normalizedMessages: result.normalizedMessages,
          };
        });
        break;
      case "flush":
        await runResult(alias, step, async () => {
          const stats = await service.flush({ reason: step.reason ?? "manual" });
          return { stats: pickStats(stats) };
        });
        break;
      case "list":
        await runResult(alias, step, async () => ({
          entries: service.list({}).map(pickListEntry),
        }));
        break;
      case "files":
        await runResult(alias, step, async () => ({
          files: walk(service.memoryDir).sort((a, b) => a.path.localeCompare(b.path)),
        }));
        break;
      case "overview":
        await runResult(alias, step, async () => ({
          overview: pickOverview(service.overview()),
        }));
        break;
      case "retrieve":
        await runResult(alias, step, async () => {
          const result = await service.retrieveContext(step.query, {});
          return pickRetrieval(result);
        });
        break;
      case "search":
        await runResult(alias, step, async () => {
          const result = await service.search(step.query, {});
          return { ...pickRetrieval(result), systemContext: buildMemoryRecallSystemContext(result.context ?? "") };
        });
        break;
      case "dream":
        await runResult(alias, step, async () => {
          const result = await service.dream(step.trigger ?? "manual");
          return pickDream(result);
        });
        break;
      case "rollback":
        await runResult(alias, step, async () => {
          try {
            const result = service.rollbackLastDream();
            return {
              ok: true,
              rolledBackAt: result.rolledBackAt,
              snapshotCapturedAt: result.snapshotCapturedAt,
              restored: result.restored,
            };
          } catch (error) {
            // Expected when no dream snapshot is installed; reported honestly.
            return { ok: false, error: String(error) };
          }
        });
        break;
      default:
        throw new Error(`unknown op ${step.op}`);
    }
  }

  // Snapshot memory files and L0 rows after the service is closed so sqlite is
  // fully flushed and the artifact is stable.
  for (const service of services.values()) service.close();
  for (const [alias, entry] of Object.entries(workspaces)) {
    entry.l0 = readL0(join(entry.dataDir, "control.sqlite"));
    entry.files = walk(entry.memoryDir);
    entry.files.sort((a, b) => a.path.localeCompare(b.path));
  }

  const globalFiles = walk(join(tsRoot, "global")).sort((a, b) => a.path.localeCompare(b.path));
  const artifact = {
    side: "typescript",
    scenario: options.scenario,
    title: spec.title,
    nodeVersion: process.version,
    tsModule: modulePath,
    controlPlane: { format: "sqlite", fileName: "control.sqlite" },
    workspaces,
    globalFiles,
  };
  const serialized = `${JSON.stringify(artifact, null, 2)}\n`;
  if (options.out === "-") {
    // `--out -` streams the artifact JSON to stdout (one scenario per invocation).
    process.stderr.write(`ts_runner: ${options.scenario} -> <stdout>\n`);
    process.stdout.write(serialized);
  } else {
    mkdirSync(dirname(options.out), { recursive: true });
    writeFileSync(options.out, serialized);
    process.stdout.write(`ts_runner: ${options.scenario} -> ${options.out}\n`);
  }
  if (failures.length > 0) {
    process.stderr.write(`ts_runner: ${failures.length} step(s) failed:\n${failures.join("\n")}\n`);
    process.exit(1);
  }
}

main().catch((error) => {
  process.stderr.write(`ts_runner: fatal: ${error?.stack ?? error}\n`);
  process.exit(2);
});
