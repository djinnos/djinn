#!/usr/bin/env node
// Regenerate every committed artifact derived from the MCP tool schemas.
//
//   make tool-goldens          regenerate in place
//   make tool-goldens-check    regenerate, then fail if the tree moved
//
// The artifact set, the command that produces each one, and the order they run
// in all live in scripts/tool-goldens.manifest.json. Nothing is hard-coded here.
//
// Flags:
//   --plan        print the ordered plan as JSON and exit without running it
//   --only <id>   run a single producer (repeatable); mainly for debugging
//   --quiet       suppress per-producer progress

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";

import {
  instaSnapshotBody,
  loadManifest,
  producerOutputs,
  repoRoot,
} from "./lib/tool-goldens.mjs";

function parseArgs(argv) {
  const only = [];
  let plan = false;
  let quiet = false;
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--plan") plan = true;
    else if (arg === "--quiet") quiet = true;
    else if (arg === "--only") {
      i += 1;
      if (i >= argv.length) {
        throw new Error("--only requires a producer id");
      }
      only.push(argv[i]);
    } else throw new Error(`unknown argument: ${arg}`);
  }
  return { only, plan, quiet };
}

/** The ordered, side-effect-free description of what a full run would do. */
export function buildPlan(manifest, only = []) {
  const selected =
    only.length === 0
      ? manifest.producers
      : manifest.producers.filter((producer) => only.includes(producer.id));
  const unknown = only.filter((id) => !manifest.producers.some((p) => p.id === id));
  if (unknown.length > 0) {
    throw new Error(`unknown producer id(s): ${unknown.join(", ")}`);
  }
  return selected.map((producer) => ({
    id: producer.id,
    kind: producer.kind,
    description: producer.description,
    cwd: producer.cwd ?? ".",
    command: producer.kind === "shell" ? producer.command : null,
    writes: producerOutputs(manifest, producer.id),
  }));
}

/**
 * Task-run images provide pnpm under PNPM_HOME, but a shell launched outside
 * the image entrypoint can omit its bin directory from PATH. Resolve that
 * provisioned toolchain before asking a producer to install or run UI tools.
 */
export function resolvePnpmCommand(env = process.env) {
  const pathEntries = (env.PATH ?? "").split(path.delimiter).filter(Boolean);
  if (pathEntries.some((entry) => existsSync(path.join(entry, "pnpm")))) return "pnpm";

  const pnpmHome = env.PNPM_HOME;
  if (!pnpmHome) return "pnpm";
  const provisioned = path.join(pnpmHome, "bin", "pnpm");
  return existsSync(provisioned) ? provisioned : "pnpm";
}

export function withPnpmOnPath(env = process.env) {
  const pnpm = resolvePnpmCommand(env);
  if (pnpm === "pnpm") return env;
  const bin = path.dirname(pnpm);
  const entries = (env.PATH ?? "").split(path.delimiter);
  if (entries.includes(bin)) return env;
  return { ...env, PATH: [bin, ...entries.filter(Boolean)].join(path.delimiter) };
}

function ensureNodeModules(manifest, producer, quiet) {
  if (!producer.requiresNodeModules) return;
  const dir = path.join(repoRoot, producer.requiresNodeModules);
  if (existsSync(path.join(dir, "node_modules"))) return;
  if (!quiet) {
    console.log(`  ${producer.requiresNodeModules}/node_modules missing — pnpm install`);
  }
  const env = withPnpmOnPath(process.env);
  const install = spawnSync(resolvePnpmCommand(env), ["install", "--frozen-lockfile"], {
    cwd: dir,
    stdio: "inherit",
    env,
  });
  if (install.status !== 0) {
    throw new Error(
      `pnpm install --frozen-lockfile failed in ${producer.requiresNodeModules}/ ` +
        `(exit ${install.status ?? "signal"}). Install pnpm 9 and retry ${manifest.regenerateCommand}.`
    );
  }
}

function runShellProducer(manifest, producer, quiet) {
  ensureNodeModules(manifest, producer, quiet);
  const cwd = path.join(repoRoot, producer.cwd);
  const env = withPnpmOnPath({ ...process.env, ...(producer.env ?? {}) });
  const result = spawnSync(producer.command, {
    cwd,
    shell: true,
    stdio: quiet ? ["ignore", "pipe", "pipe"] : "inherit",
    env,
  });
  if (result.status !== 0) {
    if (quiet) {
      process.stderr.write(result.stdout?.toString() ?? "");
      process.stderr.write(result.stderr?.toString() ?? "");
    }
    throw new Error(
      `producer '${producer.id}' failed (exit ${result.status ?? "signal"}):\n` +
        `  cd ${producer.cwd} && ${producer.command}`
    );
  }
}

/**
 * Copy insta snapshot bodies into plain-JSON corpus fixtures. Byte-identical
 * on a second run because `instaSnapshotBody` is pure and the write is a full
 * overwrite — this is the step that makes the whole target idempotent even
 * though it crosses crate boundaries.
 */
function runDeriveProducer(producer) {
  for (const file of producer.files) {
    const from = path.join(repoRoot, file.from);
    const to = path.join(repoRoot, file.to);
    if (!existsSync(from)) {
      throw new Error(
        `producer '${producer.id}' cannot read ${file.from} — it should have been written by an earlier producer`
      );
    }
    mkdirSync(path.dirname(to), { recursive: true });
    writeFileSync(to, instaSnapshotBody(readFileSync(from, "utf8")), "utf8");
  }
}

function main() {
  const { only, plan, quiet } = parseArgs(process.argv.slice(2));
  const manifest = loadManifest();
  const steps = buildPlan(manifest, only);

  if (plan) {
    console.log(JSON.stringify({ regenerateCommand: manifest.regenerateCommand, steps }, null, 2));
    return;
  }

  const byId = new Map(manifest.producers.map((producer) => [producer.id, producer]));
  for (const [index, step] of steps.entries()) {
    const producer = byId.get(step.id);
    if (!quiet) {
      console.log(`[${index + 1}/${steps.length}] ${producer.id}: ${producer.description}`);
    }
    if (producer.kind === "shell") runShellProducer(manifest, producer, quiet);
    else runDeriveProducer(producer);
  }

  if (!quiet) {
    console.log(
      `\nRegenerated ${steps.length} tool-schema golden producer(s). ` +
        `Review the diff and commit it.`
    );
  }
}

if (process.argv[1] && import.meta.url === `file://${process.argv[1]}`) {
  try {
    main();
  } catch (error) {
    console.error(`error: ${error.message}`);
    process.exit(1);
  }
}
