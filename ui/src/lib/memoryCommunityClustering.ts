import louvain from "graphology-communities-louvain";
import type Graph from "graphology";

export interface MemoryCommunityNodeAttributes {
  label?: unknown;
  title?: unknown;
  name?: unknown;
  [key: string]: unknown;
}

export interface MemoryCommunityMetadata {
  communityId: string;
  label: string;
}

type LouvainPartition = Record<string, string | number>;

const KEYWORDS_PER_COMMUNITY = 5;

const STOP_WORDS = new Set([
  "a",
  "an",
  "and",
  "are",
  "as",
  "at",
  "be",
  "by",
  "for",
  "from",
  "in",
  "into",
  "is",
  "it",
  "its",
  "of",
  "on",
  "or",
  "over",
  "the",
  "to",
  "via",
  "with",
  "without",
  "memory",
  "note",
  "notes",
  "design",
  "adr",
  "roadmap",
]);

/**
 * Run Louvain over a memory wikilink graph and return attraction-eligible
 * community metadata for each node in a non-trivial community.
 */
export function clusterMemoryCommunities(
  graph: Graph,
): Map<string, MemoryCommunityMetadata> {
  // Louvain throws when the graph has no edges; collapse that to an empty
  // result so callers don't need to special-case the disconnected-only state.
  if (graph.order <= 1 || graph.size === 0) return new Map();

  const nodeLabels = new Map<string, string>();
  graph.forEachNode((nodeId, attributes) => {
    nodeLabels.set(
      nodeId,
      labelFromAttributes(attributes as MemoryCommunityNodeAttributes, nodeId),
    );
  });

  const partition = louvain(graph, {
    randomWalk: false,
    rng: createDeterministicRandom(),
  }) as LouvainPartition;
  const communities = new Map<string | number, string[]>();

  for (const nodeId of [...nodeLabels.keys()].sort()) {
    const communityKey = partition[nodeId];
    if (communityKey === undefined) continue;
    const members = communities.get(communityKey) ?? [];
    members.push(nodeId);
    communities.set(communityKey, members);
  }

  const intraEdgeCommunityKeys = new Set<string | number>();
  graph.forEachEdge((_edgeId, _attributes, source, target) => {
    if (source === target) return;
    const sourceCommunity = partition[source];
    const targetCommunity = partition[target];
    if (sourceCommunity !== undefined && sourceCommunity === targetCommunity) {
      intraEdgeCommunityKeys.add(sourceCommunity);
    }
  });

  const clustered = new Map<string, MemoryCommunityMetadata>();
  for (const [communityKey, members] of communities) {
    const sortedMembers = [...members].sort();
    // Drop communities that can't drive attraction: singletons (one note)
    // and partitions that the graph's edge list shows no intra-edges for.
    // Downstream code can treat absence from the map as "unclustered" and
    // skip the per-community force toward a galaxy center.
    if (sortedMembers.length <= 1 || !intraEdgeCommunityKeys.has(communityKey)) {
      continue;
    }

    // Stable ids intentionally depend on the exact sorted membership: adding or
    // removing a note changes the hash so downstream caches do not confuse old
    // and new community shapes. This matches the shape used by
    // server/crates/djinn-graph/src/communities.rs (sha2-of-sorted-member-uids
    // → first 16 hex chars), just over note ids instead of repo node keys.
    const communityId = sha256Hex(sortedMembers.join("\n")).slice(0, 16);
    const label = labelForCommunity(sortedMembers, nodeLabels);
    const metadata = { communityId, label };

    for (const nodeId of sortedMembers) {
      clustered.set(nodeId, metadata);
    }
  }

  return clustered;
}

function labelFromAttributes(
  attributes: MemoryCommunityNodeAttributes,
  fallback: string,
): string {
  for (const key of ["label", "title", "name"] as const) {
    const value = attributes[key];
    if (typeof value === "string" && value.trim().length > 0) {
      return value.trim();
    }
  }
  return fallback;
}

function labelForCommunity(
  memberIds: string[],
  nodeLabels: Map<string, string>,
): string {
  const counts = new Map<string, number>();

  for (const nodeId of memberIds) {
    for (const term of termsFromLabel(nodeLabels.get(nodeId) ?? nodeId)) {
      counts.set(term, (counts.get(term) ?? 0) + 1);
    }
  }

  const keywords = [...counts]
    .sort(([leftTerm, leftCount], [rightTerm, rightCount]) => {
      if (leftCount !== rightCount) return rightCount - leftCount;
      return leftTerm.localeCompare(rightTerm);
    })
    .slice(0, KEYWORDS_PER_COMMUNITY)
    .map(([term]) => term);

  return keywords.length > 0
    ? keywords.join(" ")
    : memberIds.slice(0, 5).join(" ");
}

function termsFromLabel(label: string): string[] {
  return label
    .normalize("NFKD")
    .toLowerCase()
    .replace(/[\u0300-\u036f]/g, "")
    .split(/[^a-z0-9]+/u)
    .filter((term) => term.length > 1 && !STOP_WORDS.has(term));
}

function sha256Hex(input: string): string {
  const bytes = new TextEncoder().encode(input);
  const words: number[] = [];
  const bitLength = bytes.length * 8;

  for (const byte of bytes) {
    words.push(byte);
  }

  words.push(0x80);
  while (words.length % 64 !== 56) words.push(0);

  const high = Math.floor(bitLength / 0x1_0000_0000);
  const low = bitLength >>> 0;
  for (const value of [high, low]) {
    words.push((value >>> 24) & 0xff);
    words.push((value >>> 16) & 0xff);
    words.push((value >>> 8) & 0xff);
    words.push(value & 0xff);
  }

  let h0 = 0x6a09e667;
  let h1 = 0xbb67ae85;
  let h2 = 0x3c6ef372;
  let h3 = 0xa54ff53a;
  let h4 = 0x510e527f;
  let h5 = 0x9b05688c;
  let h6 = 0x1f83d9ab;
  let h7 = 0x5be0cd19;

  const k = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b,
    0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01,
    0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7,
    0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
    0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152,
    0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
    0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
    0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
    0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08,
    0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f,
    0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
  ];

  for (let chunk = 0; chunk < words.length; chunk += 64) {
    const w = new Array<number>(64);
    for (let i = 0; i < 16; i += 1) {
      const offset = chunk + i * 4;
      w[i] =
        ((words[offset] ?? 0) << 24) |
        ((words[offset + 1] ?? 0) << 16) |
        ((words[offset + 2] ?? 0) << 8) |
        (words[offset + 3] ?? 0);
    }

    for (let i = 16; i < 64; i += 1) {
      const previous15 = w[i - 15] ?? 0;
      const previous2 = w[i - 2] ?? 0;
      const s0 =
        rotateRight(previous15, 7) ^
        rotateRight(previous15, 18) ^
        (previous15 >>> 3);
      const s1 =
        rotateRight(previous2, 17) ^
        rotateRight(previous2, 19) ^
        (previous2 >>> 10);
      w[i] = add32(w[i - 16] ?? 0, s0, w[i - 7] ?? 0, s1);
    }

    let a = h0;
    let b = h1;
    let c = h2;
    let d = h3;
    let e = h4;
    let f = h5;
    let g = h6;
    let h = h7;

    for (let i = 0; i < 64; i += 1) {
      const s1 = rotateRight(e, 6) ^ rotateRight(e, 11) ^ rotateRight(e, 25);
      const ch = (e & f) ^ (~e & g);
      const temp1 = add32(h, s1, ch, k[i] ?? 0, w[i] ?? 0);
      const s0 = rotateRight(a, 2) ^ rotateRight(a, 13) ^ rotateRight(a, 22);
      const maj = (a & b) ^ (a & c) ^ (b & c);
      const temp2 = add32(s0, maj);

      h = g;
      g = f;
      f = e;
      e = add32(d, temp1);
      d = c;
      c = b;
      b = a;
      a = add32(temp1, temp2);
    }

    h0 = add32(h0, a);
    h1 = add32(h1, b);
    h2 = add32(h2, c);
    h3 = add32(h3, d);
    h4 = add32(h4, e);
    h5 = add32(h5, f);
    h6 = add32(h6, g);
    h7 = add32(h7, h);
  }

  return [h0, h1, h2, h3, h4, h5, h6, h7]
    .map((word) => (word >>> 0).toString(16).padStart(8, "0"))
    .join("");
}

function createDeterministicRandom(): () => number {
  let state = 0x9e3779b9;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let value = state;
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return ((value ^ (value >>> 14)) >>> 0) / 0x1_0000_0000;
  };
}

function rotateRight(value: number, shift: number): number {
  return (value >>> shift) | (value << (32 - shift));
}

function add32(...values: number[]): number {
  return values.reduce((sum, value) => (sum + value) >>> 0, 0);
}
