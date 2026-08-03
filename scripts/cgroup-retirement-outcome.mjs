// Deterministic terminal-state classifier for cgroup-launcher retirement.
// Non-green pre-landing inputs fail closed to KEEP; candidate/post-deploy faults
// stay RECOVERY until all restoration proofs are green.
import { readFileSync } from 'node:fs';

const fail = (message) => { process.stderr.write(`REJECT cgroup-retirement outcome: ${message}\n`); process.exit(1); };
const args = process.argv.slice(2);
if (args.length !== 1) fail('usage: STATE.json');
let state;
try { state = JSON.parse(readFileSync(args[0], 'utf8')); } catch (error) { fail(`invalid JSON (${error.message})`); }
if (!state || typeof state !== 'object' || Array.isArray(state)) fail('state must be an object');
const keys = Object.keys(state).sort();
if (keys.join(',') !== 'candidate_fault,post_deploy_fault,pre_landing,restoration') fail('state has unknown or missing fields');
if (!state.pre_landing || typeof state.pre_landing !== 'object' || Array.isArray(state.pre_landing) || Object.keys(state.pre_landing).length === 0) fail('pre_landing must be a non-empty object');
if (!state.restoration || typeof state.restoration !== 'object' || Array.isArray(state.restoration) || Object.keys(state.restoration).sort().join(',') !== 'aggregate_tree,launcher_leaf,node_assets') fail('restoration must record aggregate_tree, launcher_leaf, and node_assets');
for (const [name, status] of Object.entries(state.pre_landing)) if (typeof status !== 'string') fail(`pre_landing ${name} must be a status string`);
for (const [name, status] of Object.entries(state.restoration)) if (typeof status !== 'string') fail(`restoration ${name} must be a status string`);
if (typeof state.candidate_fault !== 'boolean' || typeof state.post_deploy_fault !== 'boolean') fail('fault flags must be booleans');
const restored = Object.values(state.restoration).every((status) => status === 'green');
const fault = state.candidate_fault || state.post_deploy_fault;
const preGreen = Object.values(state.pre_landing).every((status) => status === 'green');
let outcome;
if (fault && !restored) outcome = 'RECOVERY';
else if (!preGreen || fault) outcome = 'KEEP';
else outcome = 'RETIRE';
const armed = outcome === 'KEEP' ? 'preserved-assets-armed' : outcome === 'RECOVERY' ? 'dispatch-paused' : 'one-container-dispatch-authorized';
process.stdout.write(`cgroup-retirement-outcome: ${outcome} ${armed}\n`);
