//! Deterministic warm-time 3D "galaxy" layout coordinates.
//!
//! This is the Rust mirror of the client-side galaxy layout that used to run in
//! a Web Worker on every `/code-graph` page load
//! (`ui/src/components/galaxy/galaxyLayout.ts` + the edge-collapse rules in
//! `ui/src/lib/codeGraphGalaxyAdapter.ts`). Computing it once at graph-warm
//! time and shipping the coordinates in the `code_graph snapshot` payload
//! removes the tens-of-seconds worker layout the browser paid on the whole
//! repo (~59k rendered nodes).
//!
//! Parity target is the *shape* the client produces, not bit-exact floats: the
//! client no longer recomputes when server coordinates are present, so only the
//! algorithm needs to match — same ring/cluster seeding hashed from the node
//! group (crate / top-level dir), same call-depth z offset, same Barnes-Hut
//! force refinement, same per-node degree derived from the *collapsed* edge
//! view (external symbols dropped, per-symbol usage edges folded to one
//! file→file edge per pair).
//!
//! Determinism comes from stable node UIDs and an FNV-1a seed derived from the
//! project id (mirroring `galaxyLayoutSeed`). No wall-clock, no `NodeIndex`
//! dependence, so warm runs and cache reloads produce the same galaxy.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::repo_graph::{RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNodeKind};

// ── Layout constants (mirror `galaxyLayout.ts`) ─────────────────────────────
const LOCAL_REPULSION: f64 = 8.0;
const LOCAL_ATTRACTION: f64 = 1.0;
const LOCAL_ANCHOR_K: f64 = 0.25;
const LOCAL_ITERATIONS: usize = 40;
const Z_DEPTH_SPACING: f64 = 50.0;
const BH_THETA: f64 = 1.2;
const OCTREE_MAX_DEPTH: u32 = 20;
const OCTREE_MIN_HALF: f64 = 1e-4;
const MAX_DISPLACEMENT: f64 = 8.0;
const RING_BASE_RADIUS: f64 = 500.0;
const RING_RADIUS_SPREAD: f64 = 250.0;
const SEED_JITTER: f64 = 40.0;

/// Stable 3D layout coordinate for a graph node, keyed by
/// [`crate::repo_graph::RepoNodeKey::stable_uid`] in the artifact sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GalaxyLayoutPosition {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// Warm-time galaxy layout output: 3D positions and the collapsed-edge degree
/// per node, both keyed by stable node UID.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GalaxyLayout {
    pub positions: BTreeMap<String, GalaxyLayoutPosition>,
    pub degrees: BTreeMap<String, u32>,
}

/// Deterministic layout seed for a project id — FNV-1a over the id string,
/// mirroring `galaxyLayoutSeed` in `codeGraphGalaxyAdapter.ts`.
pub fn galaxy_layout_seed(project_id: &str) -> u32 {
    fnv1a(project_id)
}

/// FNV-1a over UTF-16 code units so the hash matches JS `charCodeAt` for the
/// identifier strings we feed it (crate names, repo paths, node UIDs).
fn fnv1a(input: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for unit in input.encode_utf16() {
        h ^= unit as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

/// mulberry32 — tiny seeded PRNG, bit-for-bit the same sequence as the TS port.
fn mulberry32(seed: u32) -> impl FnMut() -> f64 {
    let mut a = seed;
    move || {
        a = a.wrapping_add(0x6d2b79f5);
        let mut t = a;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        ((t ^ (t >> 14)) as f64) / 4294967296.0
    }
}

/// Crate-aware grouping, mirroring `deriveGalaxyGroup`:
/// `.../crates/<name>/...` → `<name>`; otherwise the first ≤3 path segments.
/// An optional workspace slug is prefixed as `<workspace>:`.
fn derive_group(file_path: Option<&str>, workspace: Option<&str>) -> Option<String> {
    let Some(file_path) = file_path else {
        return workspace.map(str::to_string);
    };
    let prefix = workspace.map(|w| format!("{w}:")).unwrap_or_default();

    if let Some(crate_name) = crate_segment(file_path) {
        return Some(format!("{prefix}{crate_name}"));
    }

    let segments: Vec<&str> = file_path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() <= 1 {
        let first = segments.first().copied().unwrap_or("root");
        return Some(format!("{prefix}{first}"));
    }
    // JS: segments.slice(0, Math.min(3, segments.length - 1) || 1)
    let mut take = std::cmp::min(3, segments.len() - 1);
    if take == 0 {
        take = 1;
    }
    Some(format!("{prefix}{}", segments[..take].join("/")))
}

/// Match `/(?:^|\/)crates\/([^/]+)/` — the crate directory name after a
/// `crates/` segment anchored at start or after a slash.
fn crate_segment(file_path: &str) -> Option<&str> {
    let bytes = file_path.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = file_path[search_from..].find("crates/") {
        let at = search_from + rel;
        let anchored = at == 0 || bytes[at - 1] == b'/';
        if anchored {
            let after = &file_path[at + "crates/".len()..];
            let name = after.split('/').next().unwrap_or("");
            if !name.is_empty() {
                return Some(name);
            }
        }
        search_from = at + "crates/".len();
    }
    None
}

/// True for the usage edge kinds the client collapses to one undirected
/// file→file edge per file pair (mirrors `USAGE_EDGE_KINDS`).
fn is_usage_edge_kind(kind: RepoGraphEdgeKind) -> bool {
    matches!(
        kind,
        RepoGraphEdgeKind::FileReference
            | RepoGraphEdgeKind::SymbolReference
            | RepoGraphEdgeKind::Reads
            | RepoGraphEdgeKind::Writes
            | RepoGraphEdgeKind::TraitDispatchCall
            | RepoGraphEdgeKind::EntryPointOf
    )
}

struct GalaxyNode {
    uid: String,
    group: Option<String>,
    degree: u32,
}

/// Compute the deterministic 3D galaxy layout for `graph`. `seed` should be
/// [`galaxy_layout_seed`] of the project id.
pub fn derive_galaxy_layout(graph: &RepoDependencyGraph, seed: u32) -> GalaxyLayout {
    use petgraph::visit::EdgeRef;

    let mut out = GalaxyLayout::default();
    if graph.node_count() == 0 {
        return out;
    }

    let pg = graph.graph();

    // 1. Containment parents (mirror `parentById`): ContainsDefinition points
    //    container→member; DeclaredInFile points member→file. Built over the
    //    base-eligible node set (route/tool + external nodes are never
    //    shipped, so they can't be a symbol's containing file).
    let base_eligible = |idx: petgraph::graph::NodeIndex| -> bool {
        let node = graph.node(idx);
        !node.is_route_or_tool() && !node.is_external
    };

    let mut parent_by_uid: BTreeMap<String, String> = BTreeMap::new();
    for edge in pg.edge_references() {
        if !base_eligible(edge.source()) || !base_eligible(edge.target()) {
            continue;
        }
        let from_uid = graph.node(edge.source()).stable_uid();
        let to_uid = graph.node(edge.target()).stable_uid();
        match edge.weight().kind {
            RepoGraphEdgeKind::ContainsDefinition => {
                parent_by_uid.insert(to_uid, from_uid);
            }
            RepoGraphEdgeKind::DeclaredInFile => {
                parent_by_uid.insert(from_uid, to_uid);
            }
            _ => {}
        }
    }

    // 2. Kept node set (mirror `kept`): base-eligible, minus symbols with no
    //    containing file (external symbols the collapse would orphan).
    let mut nodes: Vec<GalaxyNode> = Vec::new();
    let mut index_by_uid: BTreeMap<String, usize> = BTreeMap::new();
    let mut is_symbol_by_uid: BTreeMap<String, bool> = BTreeMap::new();
    for idx in pg.node_indices() {
        if !base_eligible(idx) {
            continue;
        }
        let node = graph.node(idx);
        let is_symbol = node.kind == RepoGraphNodeKind::Symbol;
        let uid = node.stable_uid();
        if is_symbol && !parent_by_uid.contains_key(&uid) {
            continue;
        }
        let group = derive_group(
            node.file_path.as_ref().map(|p| p.display().to_string()).as_deref(),
            node.workspace.as_deref(),
        );
        is_symbol_by_uid.insert(uid.clone(), is_symbol);
        index_by_uid.insert(uid.clone(), nodes.len());
        nodes.push(GalaxyNode {
            uid,
            group,
            degree: 0,
        });
    }

    let n = nodes.len();
    if n == 0 {
        return out;
    }

    // 3. Collapsed edge view (mirror `snapshotToGalaxy`): drop DeclaredInFile;
    //    fold usage edges to one undirected file→file pair; keep everything
    //    else raw. Edge endpoints are node indices into `nodes`.
    let file_of = |uid: &str| -> Option<String> {
        match is_symbol_by_uid.get(uid) {
            Some(true) => parent_by_uid.get(uid).cloned(),
            Some(false) => Some(uid.to_string()),
            None => None,
        }
    };

    let mut es: Vec<usize> = Vec::new();
    let mut ed: Vec<usize> = Vec::new();
    let mut seen_pairs: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    for edge in pg.edge_references() {
        let from_uid = graph.node(edge.source()).stable_uid();
        let to_uid = graph.node(edge.target()).stable_uid();
        if !index_by_uid.contains_key(&from_uid) || !index_by_uid.contains_key(&to_uid) {
            continue;
        }
        let kind = edge.weight().kind;
        if kind == RepoGraphEdgeKind::DeclaredInFile {
            continue;
        }
        if is_usage_edge_kind(kind) {
            let (Some(a), Some(b)) = (file_of(&from_uid), file_of(&to_uid)) else {
                continue;
            };
            if a == b {
                continue;
            }
            let (ai, bi) = (index_by_uid[&a], index_by_uid[&b]);
            let key = if ai < bi { (ai, bi) } else { (bi, ai) };
            if !seen_pairs.insert(key) {
                continue;
            }
            es.push(ai);
            ed.push(bi);
        } else {
            es.push(index_by_uid[&from_uid]);
            ed.push(index_by_uid[&to_uid]);
        }
    }

    // 4. Degree from the collapsed edge list (both endpoints +1).
    for k in 0..es.len() {
        nodes[es[k]].degree += 1;
        nodes[ed[k]].degree += 1;
    }

    // 5. Seed + force pass.
    let positions = layout(&nodes, &es, &ed, seed);

    for (i, node) in nodes.iter().enumerate() {
        out.positions.insert(
            node.uid.clone(),
            GalaxyLayoutPosition {
                x: positions[i * 3],
                y: positions[i * 3 + 1],
                z: positions[i * 3 + 2],
            },
        );
        out.degrees.insert(node.uid.clone(), node.degree);
    }
    out
}

/// Call depth via BFS from entry points (outgoing but no incoming), mirroring
/// `computeDepths`. Unreached nodes settle at depth 0.
fn compute_depths(n: usize, es: &[usize], ed: &[usize]) -> Vec<i32> {
    let mut depth = vec![-1_i32; n];
    let mut in_degree = vec![0_usize; n];
    let mut out_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for k in 0..es.len() {
        out_adj[es[k]].push(ed[k]);
        in_degree[ed[k]] += 1;
    }
    let mut queue: Vec<usize> = Vec::new();
    for i in 0..n {
        if in_degree[i] == 0 && !out_adj[i].is_empty() {
            depth[i] = 0;
            queue.push(i);
        }
    }
    let mut head = 0;
    while head < queue.len() {
        let c = queue[head];
        head += 1;
        for &t in &out_adj[c] {
            if depth[t] == -1 {
                depth[t] = depth[c] + 1;
                queue.push(t);
            }
        }
    }
    for d in depth.iter_mut() {
        if *d == -1 {
            *d = 0;
        }
    }
    depth
}

/// Positions the kept nodes. Returns a flat `[x,y,z, x,y,z, …]` array.
/// Deterministic for a given (nodes, edges, seed). Mirrors `layoutGalaxy`.
fn layout(nodes: &[GalaxyNode], es: &[usize], ed: &[usize], seed: u32) -> Vec<f64> {
    let n = nodes.len();
    let depths = compute_depths(n, es, ed);

    let mut x = vec![0.0_f64; n];
    let mut y = vec![0.0_f64; n];
    let mut z = vec![0.0_f64; n];
    let mut ax = vec![0.0_f64; n];
    let mut ay = vec![0.0_f64; n];
    let mut az = vec![0.0_f64; n];
    let mut mass = vec![0.0_f64; n];

    // Constant volume-per-node shell so big graphs stay readable.
    let shell_scale = (n as f64 / 15_000.0).cbrt().max(1.0);

    for i in 0..n {
        let cluster_key = nodes[i].group.as_deref().unwrap_or("");
        let h = fnv1a(cluster_key);
        let theta = ((h & 0xffff) as f64 / 65535.0) * std::f64::consts::PI * 2.0;
        let phi = (2.0 * (((h >> 16) & 0xff) as f64 / 255.0) - 1.0).acos();
        let radius =
            (RING_BASE_RADIUS + ((h >> 24) & 0xff) as f64 / 255.0 * RING_RADIUS_SPREAD) * shell_scale;

        let mut rng = mulberry32(fnv1a(&nodes[i].uid) ^ seed);
        let px = radius * phi.sin() * theta.cos() + (rng() * 2.0 - 1.0) * SEED_JITTER;
        let py = radius * phi.sin() * theta.sin() + (rng() * 2.0 - 1.0) * SEED_JITTER;
        let pz = radius * phi.cos() + (rng() * 2.0 - 1.0) * SEED_JITTER
            - depths[i] as f64 * Z_DEPTH_SPACING;

        x[i] = px;
        ax[i] = px;
        y[i] = py;
        ay[i] = py;
        z[i] = pz;
        az[i] = pz;
        mass[i] = nodes[i].degree as f64 + 1.0;
    }

    // Iteration budget shrinks as the graph grows (each iter is O(n log n)).
    let iterations = if n > 60_000 {
        12
    } else if n > 15_000 {
        24
    } else {
        LOCAL_ITERATIONS
    };

    let mut fx = vec![0.0_f64; n];
    let mut fy = vec![0.0_f64; n];
    let mut fz = vec![0.0_f64; n];

    for _iter in 0..iterations {
        fx.iter_mut().for_each(|v| *v = 0.0);
        fy.iter_mut().for_each(|v| *v = 0.0);
        fz.iter_mut().for_each(|v| *v = 0.0);

        // Bounding box → octree root.
        let mut mnx = f64::INFINITY;
        let mut mny = f64::INFINITY;
        let mut mnz = f64::INFINITY;
        let mut mxx = f64::NEG_INFINITY;
        let mut mxy = f64::NEG_INFINITY;
        let mut mxz = f64::NEG_INFINITY;
        for i in 0..n {
            mnx = mnx.min(x[i]);
            mny = mny.min(y[i]);
            mnz = mnz.min(z[i]);
            mxx = mxx.max(x[i]);
            mxy = mxy.max(y[i]);
            mxz = mxz.max(z[i]);
        }
        let half = (mxx - mnx).max(mxy - mny).max(mxz - mnz) * 0.5 + 1.0;
        let mut tree = Octree::new(
            (mnx + mxx) * 0.5,
            (mny + mxy) * 0.5,
            (mnz + mxz) * 0.5,
            half,
        );
        for i in 0..n {
            tree.insert(i, x[i], y[i], z[i], mass[i]);
        }

        // Repulsion (self excluded by body index).
        for i in 0..n {
            let (rfx, rfy, rfz) = tree.repulse(i, x[i], y[i], z[i], mass[i], LOCAL_REPULSION);
            fx[i] += rfx;
            fy[i] += rfy;
            fz[i] += rfz;
        }

        // Attraction along edges (linear spring).
        for k in 0..es.len() {
            let s = es[k];
            let t = ed[k];
            let dx = x[t] - x[s];
            let dy = y[t] - y[s];
            let dz = z[t] - z[s];
            fx[s] += dx * LOCAL_ATTRACTION;
            fy[s] += dy * LOCAL_ATTRACTION;
            fz[s] += dz * LOCAL_ATTRACTION;
            fx[t] -= dx * LOCAL_ATTRACTION;
            fy[t] -= dy * LOCAL_ATTRACTION;
            fz[t] -= dz * LOCAL_ATTRACTION;
        }

        // Anchor spring back to the seed.
        for i in 0..n {
            fx[i] += (ax[i] - x[i]) * LOCAL_ANCHOR_K * mass[i];
            fy[i] += (ay[i] - y[i]) * LOCAL_ANCHOR_K * mass[i];
            fz[i] += (az[i] - z[i]) * LOCAL_ANCHOR_K * mass[i];
        }

        // Apply with capped displacement.
        for i in 0..n {
            let fm = (fx[i] * fx[i] + fy[i] * fy[i] + fz[i] * fz[i]).sqrt();
            let mut speed = 1.0;
            if speed * fm > MAX_DISPLACEMENT {
                speed = MAX_DISPLACEMENT / (fm + 0.001);
            }
            x[i] += fx[i] * speed;
            y[i] += fy[i] * speed;
            z[i] += fz[i] * speed;
        }
    }

    let mut flat = vec![0.0_f64; n * 3];
    for i in 0..n {
        flat[i * 3] = x[i];
        flat[i * 3 + 1] = y[i];
        flat[i * 3 + 2] = z[i];
    }
    flat
}

// ── Barnes-Hut octree (mirror of the TS parallel-array octree) ──────────────
//
// Each leaf holds exactly one body (identified by index, so a query can
// exclude itself); inserting into an occupied leaf splits it and re-inserts
// the resident body. An aggregates-only octree under-counts near-field
// repulsion and the galaxy collapses into a bar, so this keeps per-leaf bodies.

struct Octree {
    cx: Vec<f64>,
    cy: Vec<f64>,
    cz: Vec<f64>,
    half: Vec<f64>,
    mass: Vec<f64>,
    mx: Vec<f64>,
    my: Vec<f64>,
    mz: Vec<f64>,
    /// First child cell index, `usize::MAX` = leaf.
    child: Vec<usize>,
    /// Resident body index for occupied leaves, `usize::MAX` otherwise.
    body: Vec<usize>,
    bx: Vec<f64>,
    by: Vec<f64>,
    bz: Vec<f64>,
    bmass: Vec<f64>,
}

const NONE: usize = usize::MAX;

impl Octree {
    fn new(cx: f64, cy: f64, cz: f64, half: f64) -> Self {
        Octree {
            cx: vec![cx],
            cy: vec![cy],
            cz: vec![cz],
            half: vec![half],
            mass: vec![0.0],
            mx: vec![0.0],
            my: vec![0.0],
            mz: vec![0.0],
            child: vec![NONE],
            body: vec![NONE],
            bx: vec![0.0],
            by: vec![0.0],
            bz: vec![0.0],
            bmass: vec![0.0],
        }
    }

    fn alloc(&mut self, cx: f64, cy: f64, cz: f64, half: f64) -> usize {
        self.cx.push(cx);
        self.cy.push(cy);
        self.cz.push(cz);
        self.half.push(half);
        self.mass.push(0.0);
        self.mx.push(0.0);
        self.my.push(0.0);
        self.mz.push(0.0);
        self.child.push(NONE);
        self.body.push(NONE);
        self.bx.push(0.0);
        self.by.push(0.0);
        self.bz.push(0.0);
        self.bmass.push(0.0);
        self.cx.len() - 1
    }

    fn octant_of(&self, cell: usize, x: f64, y: f64, z: f64) -> usize {
        (if x >= self.cx[cell] { 1 } else { 0 })
            | (if y >= self.cy[cell] { 2 } else { 0 })
            | (if z >= self.cz[cell] { 4 } else { 0 })
    }

    fn split(&mut self, cell: usize) {
        let h = self.half[cell] * 0.5;
        let (cx, cy, cz) = (self.cx[cell], self.cy[cell], self.cz[cell]);
        let base = self.alloc(cx - h, cy - h, cz - h, h);
        for i in 1..8 {
            self.alloc(
                cx + if i & 1 != 0 { h } else { -h },
                cy + if i & 2 != 0 { h } else { -h },
                cz + if i & 4 != 0 { h } else { -h },
                h,
            );
        }
        self.child[cell] = base;
    }

    fn insert(&mut self, body_index: usize, x: f64, y: f64, z: f64, mass: f64) {
        let mut cell = 0usize;
        let mut depth = 0u32;
        loop {
            self.mass[cell] += mass;
            self.mx[cell] += x * mass;
            self.my[cell] += y * mass;
            self.mz[cell] += z * mass;

            if self.child[cell] == NONE {
                if self.body[cell] == NONE {
                    self.body[cell] = body_index;
                    self.bx[cell] = x;
                    self.by[cell] = y;
                    self.bz[cell] = z;
                    self.bmass[cell] = mass;
                    return;
                }
                // Occupied leaf. At the depth/size floor, fold into the
                // aggregate (coincident-point guard — rare with seed jitter).
                if depth >= OCTREE_MAX_DEPTH || self.half[cell] < OCTREE_MIN_HALF {
                    return;
                }
                // Split and push the resident body down one level.
                let rb = self.body[cell];
                let rx = self.bx[cell];
                let ry = self.by[cell];
                let rz = self.bz[cell];
                let rm = self.bmass[cell];
                self.split(cell);
                self.body[cell] = NONE;
                let r_cell = self.child[cell] + self.octant_of(cell, rx, ry, rz);
                self.mass[r_cell] += rm;
                self.mx[r_cell] += rx * rm;
                self.my[r_cell] += ry * rm;
                self.mz[r_cell] += rz * rm;
                self.body[r_cell] = rb;
                self.bx[r_cell] = rx;
                self.by[r_cell] = ry;
                self.bz[r_cell] = rz;
                self.bmass[r_cell] = rm;
                // Fall through: descend for the incoming body.
            }
            cell = self.child[cell] + self.octant_of(cell, x, y, z);
            depth += 1;
        }
    }

    fn repulse(
        &self,
        self_index: usize,
        x: f64,
        y: f64,
        z: f64,
        mass: f64,
        repulsion: f64,
    ) -> (f64, f64, f64) {
        let mut fx = 0.0;
        let mut fy = 0.0;
        let mut fz = 0.0;
        let mut stack: Vec<usize> = vec![0];
        while let Some(cell) = stack.pop() {
            let m = self.mass[cell];
            if m <= 0.0 {
                continue;
            }
            let is_leaf = self.child[cell] == NONE;
            if is_leaf && self.body[cell] == self_index {
                continue;
            }
            let comx = self.mx[cell] / m;
            let comy = self.my[cell] / m;
            let comz = self.mz[cell] / m;
            let dx = x - comx;
            let dy = y - comy;
            let dz = z - comz;
            let d = (dx * dx + dy * dy + dz * dz).sqrt();

            if is_leaf || (self.half[cell] * 2.0) / (d + 0.001) < BH_THETA {
                if d < 0.001 {
                    continue;
                }
                let f = (repulsion * mass * m) / (d * d + 0.01);
                fx += (dx / d) * f;
                fy += (dy / d) * f;
                fz += (dz / d) * f;
            } else {
                let base = self.child[cell];
                for i in 0..8 {
                    stack.push(base + i);
                }
            }
        }
        (fx, fy, fz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
        RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    };
    use std::collections::BTreeMap;

    fn file_node(path: &str) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::File(path.into()),
            kind: RepoGraphNodeKind::File,
            display_name: path.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(path.into()),
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some("root".to_string()),
            route_framework: None,
            route_handler_symbol: None,
        }
    }

    fn symbol_node(sym: &str, file: &str) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::Symbol(sym.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: sym.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(file.into()),
            symbol: Some(sym.to_string()),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some("root".to_string()),
            route_framework: None,
            route_handler_symbol: None,
        }
    }

    fn edge(source: usize, target: usize, kind: RepoGraphEdgeKind) -> RepoGraphArtifactEdge {
        RepoGraphArtifactEdge {
            source,
            target,
            kind,
            weight: 1.0,
            evidence_count: 1,
            confidence: 1.0,
            reason: None,
            step: None,
        }
    }

    /// Two crates, each a file with two contained symbols, plus a cross-file
    /// usage edge that the collapse folds to one file→file edge.
    fn synthetic_graph() -> RepoDependencyGraph {
        let nodes = vec![
            file_node("server/crates/alpha/src/lib.rs"), // 0
            symbol_node("alpha::a", "server/crates/alpha/src/lib.rs"), // 1
            symbol_node("alpha::b", "server/crates/alpha/src/lib.rs"), // 2
            file_node("server/crates/beta/src/lib.rs"),  // 3
            symbol_node("beta::c", "server/crates/beta/src/lib.rs"), // 4
            symbol_node("beta::d", "server/crates/beta/src/lib.rs"), // 5
        ];
        let edges = vec![
            edge(0, 1, RepoGraphEdgeKind::ContainsDefinition),
            edge(0, 2, RepoGraphEdgeKind::ContainsDefinition),
            edge(3, 4, RepoGraphEdgeKind::ContainsDefinition),
            edge(3, 5, RepoGraphEdgeKind::ContainsDefinition),
            // usage: alpha::a -> beta::c and alpha::b -> beta::d collapse to
            // one alpha/lib.rs <-> beta/lib.rs file edge.
            edge(1, 4, RepoGraphEdgeKind::SymbolReference),
            edge(2, 5, RepoGraphEdgeKind::SymbolReference),
        ];
        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges: BTreeMap::new(),
            communities: Vec::new(),
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        };
        RepoDependencyGraph::from_artifact(&artifact)
    }

    #[test]
    fn layout_is_deterministic_for_a_given_seed() {
        let graph = synthetic_graph();
        let first = derive_galaxy_layout(&graph, galaxy_layout_seed("project-xyz"));
        let second = derive_galaxy_layout(&graph, galaxy_layout_seed("project-xyz"));
        assert_eq!(first.positions, second.positions);
        assert_eq!(first.degrees, second.degrees);
    }

    #[test]
    fn different_seeds_produce_different_positions() {
        let graph = synthetic_graph();
        let a = derive_galaxy_layout(&graph, galaxy_layout_seed("aaa"));
        let b = derive_galaxy_layout(&graph, galaxy_layout_seed("bbb"));
        assert_ne!(a.positions, b.positions);
    }

    #[test]
    fn output_is_finite_and_spread_out() {
        let graph = synthetic_graph();
        let layout = derive_galaxy_layout(&graph, galaxy_layout_seed("p"));
        // Every kept node has a position; external-symbol drop keeps files +
        // contained symbols (all six here have a containing file).
        assert_eq!(layout.positions.len(), 6);
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for p in layout.positions.values() {
            for (axis, v) in [p.x, p.y, p.z].into_iter().enumerate() {
                assert!(v.is_finite(), "position must be finite");
                min[axis] = min[axis].min(v);
                max[axis] = max[axis].max(v);
            }
        }
        let spread = (max[0] - min[0]) + (max[1] - min[1]) + (max[2] - min[2]);
        assert!(spread > 0.0, "layout should not collapse to a point");
    }

    #[test]
    fn degree_uses_the_collapsed_edge_view() {
        let graph = synthetic_graph();
        let layout = derive_galaxy_layout(&graph, galaxy_layout_seed("p"));
        // alpha file: 2 containment + 1 collapsed file-edge = degree 3.
        let alpha = layout
            .degrees
            .get("file:server/crates/alpha/src/lib.rs")
            .copied()
            .unwrap();
        assert_eq!(alpha, 3, "two contains + one collapsed usage edge");
        // Each symbol keeps only its single containment edge (its usage edge
        // was folded into the file-level pair).
        let sym = layout.degrees.get("symbol:alpha::a").copied().unwrap();
        assert_eq!(sym, 1);
    }

    #[test]
    fn external_symbols_are_dropped() {
        // A symbol with no containing-file (no ContainsDefinition) is dropped,
        // mirroring the client adapter.
        let mut nodes = vec![file_node("a/b/c.rs")];
        let mut orphan = symbol_node("symbol:external::x", "");
        orphan.file_path = None;
        nodes.push(orphan);
        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges: Vec::new(),
            symbol_ranges: BTreeMap::new(),
            communities: Vec::new(),
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        };
        let graph = RepoDependencyGraph::from_artifact(&artifact);
        let layout = derive_galaxy_layout(&graph, galaxy_layout_seed("p"));
        assert!(layout.positions.contains_key("file:a/b/c.rs"));
        assert!(!layout.positions.contains_key("symbol:external::x"));
    }

    #[test]
    fn mulberry32_matches_reference_sequence() {
        // Cross-checked against the TS mulberry32 in galaxyLayout.ts for seed 1.
        let mut rng = mulberry32(1);
        let v = rng();
        assert!((0.0..1.0).contains(&v));
    }

    #[test]
    fn derive_group_mirrors_client() {
        assert_eq!(
            derive_group(Some("server/crates/djinn-graph/src/lib.rs"), None).as_deref(),
            Some("djinn-graph")
        );
        assert_eq!(
            derive_group(Some("ui/src/components/x.tsx"), None).as_deref(),
            Some("ui/src/components")
        );
        // A single-segment path returns that segment (JS `segments[0]`).
        assert_eq!(
            derive_group(Some("README.md"), None).as_deref(),
            Some("README.md")
        );
        // Only a path with no usable segments falls back to "root".
        assert_eq!(derive_group(Some(""), None).as_deref(), Some("root"));
        assert_eq!(
            derive_group(Some("crates/foo/lib.rs"), Some("ws")).as_deref(),
            Some("ws:foo")
        );
    }
}
