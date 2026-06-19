//! Static SVG export with a deterministic Fruchterman-Reingold layout (no RNG —
//! positions are seeded on a circle and relaxed by fixed iterations, so the
//! output is reproducible). Repulsion uses a Barnes–Hut quadtree (O(n log n)),
//! so the layout scales to a few thousand nodes (`MAX_NODES`); labels are
//! suppressed past `LABEL_MAX_NODES` to keep large SVGs readable. Node size
//! scales with degree, color with community.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use codegraph_core::{Confidence, Node, NodeId};
use codegraph_graph::KnowledgeGraph;

use crate::common::degrees;

/// Hover tooltip text for a node: kind + the SQL facts that matter (dialect, type,
/// PK/FK, RLS), then the source file.
fn node_tooltip(n: &Node, kind: &str) -> String {
    let mut t = format!("{} \u{b7} {kind}", n.label);
    let s = |k: &str| n.extra.get(k).and_then(|v| v.as_str());
    let b = |k: &str| n.extra.get(k).and_then(|v| v.as_bool()) == Some(true);
    if let Some(d) = s("dialect") {
        t.push_str(&format!(" \u{b7} {d}"));
    }
    if let Some(dt) = s("data_type") {
        t.push_str(&format!(" \u{b7} {dt}"));
    }
    if b("pk") {
        t.push_str(" \u{b7} PK");
    }
    if let Some(fk) = s("fk_target") {
        t.push_str(&format!(" \u{b7} FK->{fk}"));
    }
    if b("rls_enabled") {
        t.push_str(" \u{b7} RLS");
    }
    if b("security_invoker") {
        t.push_str(" \u{b7} security_invoker");
    }
    if !n.source_file.is_empty() {
        t.push_str(&format!(" ({})", n.source_file));
    }
    t
}

/// Cap on laid-out nodes. Barnes–Hut keeps the layout O(n log n), so this is a
/// bound on SVG *file size* / browser render cost, not the layout math; larger
/// graphs are truncated.
const MAX_NODES: usize = 5000;
/// Above this many laid-out nodes, per-node text labels become unreadable
/// clutter and bloat the file, so only circles + edges are drawn.
const LABEL_MAX_NODES: usize = 500;
const ITERATIONS: usize = 60;
const W: f64 = 1200.0;
const H: f64 = 840.0;
const MARGIN: f64 = 40.0;

/// Barnes–Hut accuracy parameter: cells with `width/dist < THETA` are
/// approximated. Smaller = more accurate, slower. 0.5 is the standard choice.
const THETA: f64 = 0.5;
/// Stop subdividing past this depth so (near-)coincident points can't recurse
/// forever; such points share a leaf bucket instead.
const MAX_DEPTH: usize = 64;
/// At or below this many nodes, exact O(n²) repulsion is used — it has a lower
/// constant than Barnes–Hut and is fast at this scale. Above it, Barnes–Hut's
/// O(n log n) wins. (Crossover measured ≈ 800.)
const HYBRID_THRESHOLD: usize = 1000;

/// One cell of the quadtree. Internal cells have `children`; leaves carry their
/// points as `(index, x, y)` (normally one; more only at `MAX_DEPTH`).
struct QNode {
    /// Cell center + half-side (square cells).
    cx: f64,
    cy: f64,
    half: f64,
    /// Aggregate of all points in this subtree.
    mass: f64,
    com_x: f64,
    com_y: f64,
    points: Vec<(usize, f64, f64)>,
    children: Option<[usize; 4]>,
}

impl QNode {
    fn leaf(cx: f64, cy: f64, half: f64) -> Self {
        QNode {
            cx,
            cy,
            half,
            mass: 0.0,
            com_x: 0.0,
            com_y: 0.0,
            points: Vec::new(),
            children: None,
        }
    }
}

/// Barnes–Hut quadtree over a set of 2D positions, for O(n log n) repulsion.
/// Arena-based (children are indices, not boxes). Built deterministically by
/// inserting points in index order; traversal order is fixed.
struct QuadTree {
    arena: Vec<QNode>,
}

impl QuadTree {
    /// Build a quadtree over `pos` (points inserted in index order).
    fn build(pos: &[(f64, f64)]) -> Self {
        if pos.is_empty() {
            return QuadTree { arena: Vec::new() };
        }
        let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for &(x, y) in pos {
            minx = minx.min(x);
            miny = miny.min(y);
            maxx = maxx.max(x);
            maxy = maxy.max(y);
        }
        let cx = (minx + maxx) / 2.0;
        let cy = (miny + maxy) / 2.0;
        // Half-side covers the larger extent; epsilon keeps boundary points in.
        let half = ((maxx - minx).max(maxy - miny) / 2.0).max(1e-9) + 1e-9;

        let mut arena = vec![QNode::leaf(cx, cy, half)];
        for (i, &(x, y)) in pos.iter().enumerate() {
            Self::insert(&mut arena, 0, i, x, y, 0);
        }
        QuadTree { arena }
    }

    /// Which child quadrant `(x, y)` falls into, relative to a cell center.
    fn quadrant(cx: f64, cy: f64, x: f64, y: f64) -> usize {
        (if x >= cx { 1 } else { 0 }) | (if y >= cy { 2 } else { 0 })
    }

    /// Split a leaf into four empty children, returning their arena indices.
    fn subdivide(arena: &mut Vec<QNode>, node_idx: usize) -> [usize; 4] {
        let (cx, cy, h) = {
            let n = &arena[node_idx];
            (n.cx, n.cy, n.half / 2.0)
        };
        let mut idx = [0usize; 4];
        for (q, slot) in idx.iter_mut().enumerate() {
            let ccx = if q & 1 == 1 { cx + h } else { cx - h };
            let ccy = if q & 2 == 2 { cy + h } else { cy - h };
            *slot = arena.len();
            arena.push(QNode::leaf(ccx, ccy, h));
        }
        arena[node_idx].children = Some(idx);
        idx
    }

    fn insert(arena: &mut Vec<QNode>, node_idx: usize, p: usize, px: f64, py: f64, depth: usize) {
        // Fold this point into the cell's aggregate center of mass.
        {
            let n = &mut arena[node_idx];
            let new_mass = n.mass + 1.0;
            n.com_x = (n.com_x * n.mass + px) / new_mass;
            n.com_y = (n.com_y * n.mass + py) / new_mass;
            n.mass = new_mass;
        }

        if let Some(children) = arena[node_idx].children {
            let (cx, cy) = (arena[node_idx].cx, arena[node_idx].cy);
            let child = children[Self::quadrant(cx, cy, px, py)];
            Self::insert(arena, child, p, px, py, depth + 1);
            return;
        }

        // Leaf. Empty, or a max-depth bucket: just hold the point.
        if arena[node_idx].points.is_empty() || depth >= MAX_DEPTH {
            arena[node_idx].points.push((p, px, py));
            return;
        }

        // Occupied leaf: subdivide and push existing point(s) + the new one down.
        // Their mass/com were already folded into this cell above, so the
        // recursive insert only touches the children.
        let existing = std::mem::take(&mut arena[node_idx].points);
        let children = Self::subdivide(arena, node_idx);
        let (cx, cy) = (arena[node_idx].cx, arena[node_idx].cy);
        for (e, ex, ey) in existing {
            Self::insert(
                arena,
                children[Self::quadrant(cx, cy, ex, ey)],
                e,
                ex,
                ey,
                depth + 1,
            );
        }
        Self::insert(
            arena,
            children[Self::quadrant(cx, cy, px, py)],
            p,
            px,
            py,
            depth + 1,
        );
    }

    /// Net repulsion force on point `i` (at `pos_i`): cells with
    /// `width / dist < theta` are approximated as a point mass at their center
    /// of mass; otherwise recurse. Leaves contribute exact pairwise force,
    /// skipping self. `k` is the FR ideal edge length.
    fn repulsion(&self, i: usize, pos_i: (f64, f64), k: f64, theta: f64) -> (f64, f64) {
        let mut f = (0.0, 0.0);
        if !self.arena.is_empty() {
            self.accumulate(0, i, pos_i, k, theta, &mut f);
        }
        f
    }

    fn accumulate(
        &self,
        node_idx: usize,
        i: usize,
        pos_i: (f64, f64),
        k: f64,
        theta: f64,
        f: &mut (f64, f64),
    ) {
        let node = &self.arena[node_idx];
        if node.mass == 0.0 {
            return;
        }
        match node.children {
            None => {
                for &(j, jx, jy) in &node.points {
                    if j == i {
                        continue;
                    }
                    let dx = pos_i.0 - jx;
                    let dy = pos_i.1 - jy;
                    let dist = (dx * dx + dy * dy).sqrt().max(0.01);
                    let force = k * k / dist;
                    f.0 += dx / dist * force;
                    f.1 += dy / dist * force;
                }
            }
            Some(children) => {
                let dx = pos_i.0 - node.com_x;
                let dy = pos_i.1 - node.com_y;
                let dist = (dx * dx + dy * dy).sqrt().max(0.01);
                if (node.half * 2.0) / dist < theta {
                    let force = k * k * node.mass / dist;
                    f.0 += dx / dist * force;
                    f.1 += dy / dist * force;
                } else {
                    for &c in &children {
                        self.accumulate(c, i, pos_i, k, theta, f);
                    }
                }
            }
        }
    }
}

/// Connected components over an undirected edge set on nodes `0..n`. Returns
/// each component as a sorted index list, components ordered by smallest member.
/// Deterministic (BFS in index order).
fn connected_components(n: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(u, v) in edges {
        if u < n && v < n && u != v {
            adj[u].push(v);
            adj[v].push(u);
        }
    }
    let mut seen = vec![false; n];
    let mut comps = Vec::new();
    for start in 0..n {
        if seen[start] {
            continue;
        }
        let mut comp = Vec::new();
        let mut queue = std::collections::VecDeque::from([start]);
        seen[start] = true;
        while let Some(u) = queue.pop_front() {
            comp.push(u);
            for &w in &adj[u] {
                if !seen[w] {
                    seen[w] = true;
                    queue.push_back(w);
                }
            }
        }
        comp.sort_unstable();
        comps.push(comp);
    }
    comps
}

/// Shelf-pack one square cell per component into a roughly `aspect`-ratio
/// arrangement. Cell side ∝ √size (area ∝ node count). Returns `(x, y, side)`
/// per input index; cells never overlap. Deterministic.
fn pack_cells(sizes: &[usize], aspect: f64) -> Vec<(f64, f64, f64)> {
    let n = sizes.len();
    if n == 0 {
        return Vec::new();
    }
    let side = |s: usize| (s as f64).sqrt().max(1.0);

    // Place largest-first (better packing); keep original index for the result.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        side(sizes[b])
            .partial_cmp(&side(sizes[a]))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });

    let total_area: f64 = sizes.iter().map(|&s| side(s) * side(s)).sum();
    let max_side = sizes.iter().map(|&s| side(s)).fold(0.0_f64, f64::max);
    let target_w = (total_area * aspect).sqrt().max(max_side);

    let mut result = vec![(0.0, 0.0, 0.0); n];
    let (mut x, mut y, mut row_h) = (0.0_f64, 0.0_f64, 0.0_f64);
    for &i in &order {
        let s = side(sizes[i]);
        if x > 0.0 && x + s > target_w {
            y += row_h; // new shelf
            x = 0.0;
            row_h = 0.0;
        }
        result[i] = (x, y, s);
        x += s;
        row_h = row_h.max(s);
    }
    result
}

/// Lay out a graph by connected component, each relaxed independently and then
/// shelf-packed into its own cell, so disconnected pieces occupy disjoint
/// regions instead of one piece's outliers crushing the rest. Returns positions
/// in packed (un-normalized) space, in node-index order.
fn packed_layout(n: usize, edges: &[(usize, usize)]) -> Vec<(f64, f64)> {
    if n == 0 {
        return Vec::new();
    }
    let comps = connected_components(n, edges);
    let sizes: Vec<usize> = comps.iter().map(|c| c.len()).collect();
    let cells = pack_cells(&sizes, W / H);

    // Margin kept inside each cell so packed components are visually separated.
    const INSET: f64 = 0.08;
    let mut out = vec![(0.0, 0.0); n];
    for (ci, comp) in comps.iter().enumerate() {
        let cn = comp.len();
        // Reindex this component's nodes to 0..cn, keeping only internal edges.
        let local_of: HashMap<usize, usize> =
            comp.iter().enumerate().map(|(li, &g)| (g, li)).collect();
        let cedges: Vec<(usize, usize)> = edges
            .iter()
            .filter_map(|&(u, v)| Some((*local_of.get(&u)?, *local_of.get(&v)?)))
            .collect();

        let mut local = force_layout(cn, &cedges);
        normalize_unit(&mut local);

        let (cx, cy, side) = cells[ci];
        let avail = side * (1.0 - 2.0 * INSET);
        let pad = side * INSET;
        for (li, &g) in comp.iter().enumerate() {
            out[g] = (
                cx + pad + local[li].0 * avail,
                cy + pad + local[li].1 * avail,
            );
        }
    }
    out
}

/// Scale positions in place to fit `[0,1] × [0,1]`; a degenerate (single point
/// or coincident) set collapses to the center.
fn normalize_unit(pos: &mut [(f64, f64)]) {
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &(x, y) in pos.iter() {
        minx = minx.min(x);
        miny = miny.min(y);
        maxx = maxx.max(x);
        maxy = maxy.max(y);
    }
    let (dx, dy) = (maxx - minx, maxy - miny);
    for p in pos.iter_mut() {
        p.0 = if dx > 1e-9 { (p.0 - minx) / dx } else { 0.5 };
        p.1 = if dy > 1e-9 { (p.1 - miny) / dy } else { 0.5 };
    }
}

/// Deterministic Fruchterman–Reingold relaxation of `n` nodes (seeded on a unit
/// circle) connected by `edges`. Returns raw positions (un-normalized).
fn force_layout(n: usize, edges: &[(usize, usize)]) -> Vec<(f64, f64)> {
    if n == 0 {
        return Vec::new();
    }
    let mut pos: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let theta = std::f64::consts::TAU * (i as f64) / (n as f64);
            (theta.cos(), theta.sin())
        })
        .collect();

    let area = W * H;
    let k = (area / n as f64).sqrt();
    for it in 0..ITERATIONS {
        // Repulsion: exact below HYBRID_THRESHOLD, Barnes–Hut above.
        let mut disp = repulsion(&pos, k);
        // Attraction along edges.
        for &(u, v) in edges {
            let dx = pos[u].0 - pos[v].0;
            let dy = pos[u].1 - pos[v].1;
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let force = dist * dist / k;
            let (ux, uy) = (dx / dist, dy / dist);
            disp[u].0 -= ux * force;
            disp[u].1 -= uy * force;
            disp[v].0 += ux * force;
            disp[v].1 += uy * force;
        }
        // Cool down and apply, capped by temperature.
        let temp = (W / 10.0) * (1.0 - it as f64 / ITERATIONS as f64);
        for (p, &(dx, dy)) in pos.iter_mut().zip(disp.iter()) {
            let dl = (dx * dx + dy * dy).sqrt().max(0.01);
            p.0 += dx / dl * dl.min(temp);
            p.1 += dy / dl * dl.min(temp);
        }
    }
    pos
}

/// Per-node repulsion displacement for one FR iteration. Hybrid: exact O(n²)
/// all-pairs for `n <= HYBRID_THRESHOLD` (lower constant, exact), Barnes–Hut
/// O(n log n) above it. Both are deterministic.
fn repulsion(pos: &[(f64, f64)], k: f64) -> Vec<(f64, f64)> {
    let n = pos.len();
    let mut disp = vec![(0.0, 0.0); n];
    if n <= HYBRID_THRESHOLD {
        // Exact all-pairs, each pair computed once and applied symmetrically.
        // Range loop is intentional: each pair mutates both `disp[i]` and
        // `disp[j]`, which an iterator can't express.
        #[allow(clippy::needless_range_loop)]
        for i in 0..n {
            for j in (i + 1)..n {
                let dx = pos[i].0 - pos[j].0;
                let dy = pos[i].1 - pos[j].1;
                let dist = (dx * dx + dy * dy).sqrt().max(0.01);
                let force = k * k / dist;
                let (ux, uy) = (dx / dist, dy / dist);
                disp[i].0 += ux * force;
                disp[i].1 += uy * force;
                disp[j].0 -= ux * force;
                disp[j].1 -= uy * force;
            }
        }
    } else {
        let tree = QuadTree::build(pos);
        for (i, d) in disp.iter_mut().enumerate() {
            let (fx, fy) = tree.repulsion(i, pos[i], k, THETA);
            d.0 += fx;
            d.1 += fy;
        }
    }
    disp
}

/// Deterministic FR layout over the first `MAX_NODES` nodes; returns positions
/// in SVG coordinates plus the node ids that were laid out (graph order).
fn layout(kg: &KnowledgeGraph) -> (Vec<NodeId>, Vec<(f64, f64)>) {
    // When truncating, keep structural/code nodes over columns: columns dominate
    // SQL graphs and are the least informative individually, so they should be the
    // first dropped rather than crowding out tables and call structure.
    let ids: Vec<NodeId> = if kg.node_count() <= MAX_NODES {
        kg.nodes().map(|n| n.id.clone()).collect()
    } else {
        let mut ns: Vec<&Node> = kg.nodes().collect();
        ns.sort_by_key(|n| u8::from(crate::common::visual_kind(n) == "column"));
        ns.into_iter()
            .take(MAX_NODES)
            .map(|n| n.id.clone())
            .collect()
    };
    let n = ids.len();
    if n == 0 {
        return (ids, vec![]);
    }
    let index: HashMap<&NodeId, usize> = ids.iter().enumerate().map(|(i, id)| (id, i)).collect();
    let edges: Vec<(usize, usize)> = kg
        .edges()
        .filter_map(|e| Some((*index.get(&e.source)?, *index.get(&e.target)?)))
        .filter(|(u, v)| u != v)
        .collect();

    // Lay out per connected component and shelf-pack, so disconnected fragments
    // don't stretch the viewport and crush the main cluster.
    let mut pos = packed_layout(n, &edges);

    // Normalize to the viewport with a single (aspect-preserving) scale, centerd,
    // so components aren't stretched into ellipses.
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &(x, y) in &pos {
        minx = minx.min(x);
        miny = miny.min(y);
        maxx = maxx.max(x);
        maxy = maxy.max(y);
    }
    let (uw, uh) = (W - 2.0 * MARGIN, H - 2.0 * MARGIN);
    let s = (uw / (maxx - minx).max(1e-6)).min(uh / (maxy - miny).max(1e-6));
    let ox = MARGIN + (uw - (maxx - minx) * s) / 2.0;
    let oy = MARGIN + (uh - (maxy - miny) * s) / 2.0;
    for p in &mut pos {
        p.0 = ox + (p.0 - minx) * s;
        p.1 = oy + (p.1 - miny) * s;
    }
    (ids, pos)
}

/// Points of a regular `sides`-gon centerd at `(cx, cy)` with circumradius `r`,
/// rotated by `rot` radians — as an SVG `points` attribute value.
fn regular_polygon(cx: f64, cy: f64, r: f64, sides: usize, rot: f64) -> String {
    let mut pts = String::new();
    for i in 0..sides {
        let a = rot + std::f64::consts::TAU * (i as f64) / (sides as f64);
        if i > 0 {
            pts.push(' ');
        }
        pts.push_str(&format!("{:.1},{:.1}", cx + r * a.cos(), cy + r * a.sin()));
    }
    pts
}

/// SVG for a node, shaped by its [`crate::common::kind_shape`] token (table →
/// diamond, view → triangle, column → small dot, index/class → square, procedure
/// → hexagon, trigger → star, policy → inverted triangle, code → circle), keeping
/// the community color.
fn node_shape_svg(
    shape: &str,
    cx: f64,
    cy: f64,
    r: f64,
    color: &str,
    opacity: f64,
    extra: &str,
) -> String {
    use std::f64::consts::PI;
    let poly = |sides, rot| {
        format!(
            "<polygon points=\"{}\" fill=\"{color}\" opacity=\"{opacity}\"{extra}/>\n",
            regular_polygon(cx, cy, r, sides, rot)
        )
    };
    let circle = |rr: f64| {
        format!(
            "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"{rr:.1}\" fill=\"{color}\" opacity=\"{opacity}\"{extra}/>\n"
        )
    };
    match shape {
        "dot" => circle(r * 0.6),
        "square" => format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"{color}\" opacity=\"{opacity}\"{extra}/>\n",
            cx - r, cy - r, 2.0 * r, 2.0 * r
        ),
        "diamond" => poly(4, PI / 4.0),
        "triangle" => poly(3, -PI / 2.0),
        "triangle_down" => poly(3, PI / 2.0),
        "pentagon" => poly(5, -PI / 2.0),
        "hexagon" => poly(6, 0.0),
        "star" => star_svg(cx, cy, r, color, opacity, extra),
        _ => circle(r),
    }
}

/// A 5-point star polygon (alternating outer/inner radius).
fn star_svg(cx: f64, cy: f64, r: f64, color: &str, opacity: f64, extra: &str) -> String {
    use std::f64::consts::PI;
    let mut pts = String::new();
    for i in 0..10 {
        let rr = if i % 2 == 0 { r } else { r * 0.42 };
        let a = -PI / 2.0 + i as f64 * PI / 5.0;
        if i > 0 {
            pts.push(' ');
        }
        pts.push_str(&format!(
            "{:.1},{:.1}",
            cx + rr * a.cos(),
            cy + rr * a.sin()
        ));
    }
    format!("<polygon points=\"{pts}\" fill=\"{color}\" opacity=\"{opacity}\"{extra}/>\n")
}

/// A compact legend (backing panel + one shape row per node kind present) in the
/// lower-left, so the SQL/code shapes are self-describing.
fn legend_svg(kinds: &[&str]) -> String {
    if kinds.is_empty() {
        return String::new();
    }
    let sw = "#cfd2ff";
    let x = 16.0;
    let row_h = 16.0;
    let shown: Vec<&&str> = kinds.iter().take(14).collect();
    let top = H - (shown.len() as f64 * row_h) - 22.0;
    let mut s = format!(
        "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"140\" height=\"{:.1}\" rx=\"6\" fill=\"#14142a\" opacity=\"0.82\"/>\n",
        x - 8.0,
        top - 8.0,
        shown.len() as f64 * row_h + 16.0
    );
    for (i, kind) in shown.iter().enumerate() {
        let cy = top + i as f64 * row_h + 6.0;
        let cx = x + 6.0;
        s.push_str(&node_shape_svg(
            crate::common::kind_shape(kind),
            cx,
            cy,
            5.0,
            sw,
            0.9,
            "",
        ));
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"#cfd2ff\">{kind}</text>\n",
            x + 18.0,
            cy + 3.5
        ));
    }
    s
}

/// A legend panel (lower-right) mapping each repo tag to its ring color. Only
/// emitted for federated graphs.
fn repo_legend_svg(repos: &std::collections::BTreeMap<String, usize>) -> String {
    let row_h = 17.0;
    let panel_w = 150.0;
    let x = W - panel_w - 8.0;
    let top = H - (repos.len() as f64 * row_h) - 22.0;
    let mut s = format!(
        "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{panel_w}\" height=\"{:.1}\" rx=\"6\" fill=\"#14142a\" opacity=\"0.82\"/>\n<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"#cfd2ff\">Repos</text>\n",
        x,
        top - 8.0,
        repos.len() as f64 * row_h + 22.0,
        x + 8.0,
        top + 4.0
    );
    let mut entries: Vec<(&String, &usize)> = repos.iter().collect();
    entries.sort_by_key(|(_, i)| **i);
    for (label, i) in entries {
        let cy = top + (*i as f64 + 1.0) * row_h + 6.0;
        let cx = x + 14.0;
        let rc = crate::common::repo_color(*i);
        s.push_str(&format!(
            "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"5\" fill=\"none\" stroke=\"{rc}\" stroke-width=\"1.5\"/>\n<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"#cfd2ff\">{}</text>\n",
            x + 26.0,
            cy + 3.5,
            crate::common::xml_escape(label)
        ));
    }
    s
}

/// Render the graph as a standalone SVG document.
pub fn to_svg_string(kg: &KnowledgeGraph) -> String {
    let (ids, pos) = layout(kg);
    let show_labels = ids.len() <= LABEL_MAX_NODES;
    let pos_of: HashMap<&NodeId, (f64, f64)> = ids.iter().zip(pos.iter().copied()).collect();
    let deg = degrees(kg);
    let max_deg = deg.values().copied().max().unwrap_or(1).max(1) as f64;

    let mut body = String::new();
    body.push_str(&format!(
        "<rect width=\"{W}\" height=\"{H}\" fill=\"#1a1a2e\"/>\n"
    ));
    // Edges first (under nodes). Color reads the relation (so SQL structure and
    // code->SQL bridges stand apart from generic calls); cross-repo edges take an
    // accent + extra width; confidence still reads via the dash.
    for e in kg.edges() {
        if let (Some(&(x0, y0)), Some(&(x1, y1))) = (pos_of.get(&e.source), pos_of.get(&e.target)) {
            let dash = if e.confidence == Confidence::Extracted {
                ""
            } else {
                " stroke-dasharray=\"4 3\""
            };
            let bridge = crate::common::is_bridge_relation(&e.relation);
            let (stroke, width, opacity) = if e.cross_repo {
                (crate::common::CROSS_REPO_COLOR, 1.6, 0.7)
            } else if bridge {
                (crate::common::relation_color(&e.relation), 1.4, 0.85)
            } else {
                (
                    crate::common::relation_color(&e.relation),
                    0.8,
                    if e.confidence == Confidence::Extracted {
                        0.55
                    } else {
                        0.3
                    },
                )
            };
            body.push_str(&format!(
                "<line x1=\"{x0:.1}\" y1=\"{y0:.1}\" x2=\"{x1:.1}\" y2=\"{y1:.1}\" stroke=\"{stroke}\" stroke-width=\"{width}\" opacity=\"{opacity}\"{dash}/>\n"
            ));
        }
    }
    // Nodes + labels. Fill = community; shape = kind (table=diamond, column=dot,
    // …); ring = repo (federated); external-package stubs render dimmed.
    let repos = crate::common::repo_index(kg);
    let federated = !repos.is_empty();
    let mut kinds_present: std::collections::BTreeSet<&'static str> =
        std::collections::BTreeSet::new();
    for n in kg.nodes() {
        let Some(&(x, y)) = pos_of.get(&n.id) else {
            continue;
        };
        let color = crate::common::community_color(n.community.unwrap_or(0) as usize);
        let r = 4.0 + 10.0 * (*deg.get(&n.id).unwrap_or(&1) as f64 / max_deg);
        let kind = crate::common::visual_kind(n);
        kinds_present.insert(kind);
        let shape = crate::common::kind_shape(kind);
        // Federation styling (repo ring + dimmed external stubs) is computed ONLY
        // for federated graphs, so single-repo SVG is identical (and as fast).
        let (extra, opacity) = if federated {
            let external = crate::common::is_external_package(n);
            let ring = n
                .repo
                .as_deref()
                .and_then(|t| repos.get(t))
                .map(|&i| crate::common::repo_color(i));
            let extra = match (ring, external) {
                (Some(rc), true) => {
                    format!(" stroke=\"{rc}\" stroke-width=\"1.5\" stroke-dasharray=\"2 2\"")
                }
                (Some(rc), false) => format!(" stroke=\"{rc}\" stroke-width=\"1.5\""),
                (None, true) => {
                    " stroke=\"#888888\" stroke-width=\"1.2\" stroke-dasharray=\"2 2\"".to_string()
                }
                (None, false) => String::new(),
            };
            (extra, if external { 0.4 } else { 0.9 })
        } else {
            (String::new(), 0.9)
        };
        // Wrap the shape in a <g> with a <title> so hovering shows the kind + SQL
        // facts (the SVG previously had no tooltip at all).
        body.push_str(&format!(
            "<g><title>{}</title>{}</g>\n",
            crate::common::xml_escape(&node_tooltip(n, kind)),
            node_shape_svg(shape, x, y, r, color, opacity, &extra)
        ));
        if show_labels {
            body.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"7\" fill=\"white\">{}</text>\n",
                x + r + 2.0,
                y + 2.0,
                crate::common::xml_escape(&n.label)
            ));
        }
    }
    let kinds: Vec<&str> = kinds_present.iter().copied().collect();
    body.push_str(&legend_svg(&kinds));
    if !repos.is_empty() {
        body.push_str(&repo_legend_svg(&repos));
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{W}\" height=\"{H}\" viewBox=\"0 0 {W} {H}\">\n{body}</svg>\n"
    )
}

/// Write `graph.svg`.
pub fn to_svg(kg: &KnowledgeGraph, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, to_svg_string(kg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{kg_with_asset, sample_kg};

    #[test]
    fn node_shape_svg_picks_shape_by_kind() {
        assert!(node_shape_svg("circle", 0.0, 0.0, 5.0, "#fff", 0.9, "").starts_with("<circle"));
        assert!(node_shape_svg("dot", 0.0, 0.0, 5.0, "#fff", 0.9, "").starts_with("<circle"));
        assert!(node_shape_svg("square", 0.0, 0.0, 5.0, "#fff", 0.9, "").starts_with("<rect"));
        assert!(node_shape_svg("diamond", 0.0, 0.0, 5.0, "#fff", 0.9, "").starts_with("<polygon"));
        assert!(node_shape_svg("triangle", 0.0, 0.0, 5.0, "#fff", 0.9, "").starts_with("<polygon"));
        assert!(node_shape_svg("star", 0.0, 0.0, 5.0, "#fff", 0.9, "").starts_with("<polygon"));
    }

    #[test]
    fn asset_graph_renders_legend_and_asset_shape() {
        let svg = to_svg_string(&kg_with_asset());
        // The stylesheet asset node (community 0, #4caf50) renders as a square rect.
        assert!(
            svg.contains("<rect x=") && svg.contains("fill=\"#4caf50\""),
            "asset node should render as a colored rect"
        );
        assert!(svg.contains("<circle"), "the code node stays a circle");
        assert!(
            svg.contains(">stylesheet<"),
            "legend lists the present stylesheet kind"
        );
    }

    #[test]
    fn sql_graph_shapes_kinds_tooltips_and_relation_colors() {
        let gd: codegraph_core::GraphData = serde_json::from_value(serde_json::json!({
            "nodes": [
                {"id":"app.f","label":"f()","file_type":"code","source_file":"a.py","community":0},
                {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table","community":0,"rls_enabled":true},
                {"id":"sql:orders:col:total","label":"total","file_type":"code","source_file":"s.sql","kind":"column","community":0,"data_type":"int"}
            ],
            "links": [
                {"source":"app.f","target":"sql:orders","relation":"queries","confidence":"INFERRED","source_file":"a.py"},
                {"source":"sql:orders","target":"sql:orders:col:total","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}
            ]
        }))
        .unwrap();
        let svg = to_svg_string(&KnowledgeGraph::from_graph_data(gd));
        assert!(
            svg.contains("<polygon"),
            "table renders as a diamond polygon"
        );
        assert!(
            svg.contains(crate::common::relation_color("queries")),
            "the code->SQL bridge edge uses the bridge color"
        );
        assert!(svg.contains("<title>"), "nodes now carry hover tooltips");
        assert!(
            svg.contains(">table<") && svg.contains(">column<"),
            "legend lists the SQL kinds present"
        );
    }

    #[test]
    fn federated_svg_highlights_cross_repo_and_repos() {
        use crate::tests_support::kg_federated;
        let svg = to_svg_string(&kg_federated());
        assert!(
            svg.contains(crate::common::CROSS_REPO_COLOR),
            "cross-repo accent"
        );
        assert!(
            svg.contains(">app<") && svg.contains(">billing<"),
            "repo legend"
        );
        assert!(
            svg.contains("stroke-dasharray=\"2 2\""),
            "external dashed ring"
        );
    }

    #[test]
    fn single_repo_svg_has_no_repo_legend() {
        let svg = to_svg_string(&sample_kg());
        assert!(!svg.contains(crate::common::CROSS_REPO_COLOR));
        assert!(!svg.contains(">Repos<"));
    }

    /// Reference O(n²) repulsion, matching `layout`'s inner loop (incl. the
    /// 0.01 distance clamp). Net force pushing point `i` away from all others.
    fn brute_repulsion(pos: &[(f64, f64)], i: usize, k: f64) -> (f64, f64) {
        let mut f = (0.0, 0.0);
        for (j, &(jx, jy)) in pos.iter().enumerate() {
            if i == j {
                continue;
            }
            let dx = pos[i].0 - jx;
            let dy = pos[i].1 - jy;
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let force = k * k / dist;
            f.0 += dx / dist * force;
            f.1 += dy / dist * force;
        }
        f
    }

    #[test]
    fn connected_components_splits_disjoint_subgraphs() {
        // 0-1-2 (chain), 3-4 (pair), 5 (isolated).
        let comps = connected_components(6, &[(0, 1), (1, 2), (3, 4)]);
        assert_eq!(comps.len(), 3);
        assert!(comps.contains(&vec![0, 1, 2]));
        assert!(comps.contains(&vec![3, 4]));
        assert!(comps.contains(&vec![5]));
    }

    #[test]
    fn connected_components_ring_is_one() {
        let comps = connected_components(4, &[(0, 1), (1, 2), (2, 3), (3, 0)]);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0], vec![0, 1, 2, 3]);
    }

    fn cells_overlap(a: (f64, f64, f64), b: (f64, f64, f64)) -> bool {
        let (ax, ay, asz) = a;
        let (bx, by, bsz) = b;
        ax < bx + bsz && bx < ax + asz && ay < by + bsz && by < ay + asz
    }

    #[test]
    fn pack_cells_are_disjoint_and_size_proportional() {
        let cells = pack_cells(&[100, 50, 10, 4, 1], 1.43);
        assert_eq!(cells.len(), 5);
        // Larger component, larger cell (side proportional to sqrt(size)).
        assert!(
            cells[0].2 > cells[4].2,
            "bigger component should get a bigger cell"
        );
        // No two cells overlap.
        for i in 0..cells.len() {
            for j in (i + 1)..cells.len() {
                assert!(
                    !cells_overlap(cells[i], cells[j]),
                    "cells {i} and {j} overlap"
                );
            }
        }
    }

    #[test]
    fn packed_layout_separates_components() {
        // Two disjoint triangles: their bounding boxes must not overlap.
        let edges = vec![(0, 1), (1, 2), (2, 0), (3, 4), (4, 5), (5, 3)];
        let pos = packed_layout(6, &edges);
        let bbox = |idx: &[usize]| {
            let mut bb = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for &i in idx {
                bb.0 = bb.0.min(pos[i].0);
                bb.1 = bb.1.min(pos[i].1);
                bb.2 = bb.2.max(pos[i].0);
                bb.3 = bb.3.max(pos[i].1);
            }
            bb
        };
        let a = bbox(&[0, 1, 2]);
        let b = bbox(&[3, 4, 5]);
        // strictly disjoint on x or y (coincident degenerate boxes are NOT disjoint)
        let disjoint = a.2 < b.0 || b.2 < a.0 || a.3 < b.1 || b.3 < a.1;
        assert!(
            disjoint,
            "component bounding boxes overlap: a={a:?} b={b:?}"
        );
    }

    #[test]
    fn repulsion_is_exact_below_threshold() {
        // Below HYBRID_THRESHOLD, repulsion is exact all-pairs: net force on
        // each node equals the brute-force reference (float-order tolerance).
        let pos = vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (3.0, 7.0),
            (-5.0, 2.0),
            (1.0, -8.0),
            (6.0, 6.0),
            (-9.0, -4.0),
            (12.0, -3.0),
        ];
        assert!(pos.len() <= HYBRID_THRESHOLD);
        let k = 50.0;
        let disp = repulsion(&pos, k);
        for (i, &(hx, hy)) in disp.iter().enumerate() {
            let bf = brute_repulsion(&pos, i, k);
            assert!(
                (hx - bf.0).abs() < 1e-6 && (hy - bf.1).abs() < 1e-6,
                "node {i}: hybrid {:?} != exact {bf:?}",
                (hx, hy)
            );
        }
    }

    #[test]
    fn barnes_hut_matches_brute_force_at_theta_zero() {
        // theta = 0 forces full recursion to leaves: exact all-pairs (self excluded).
        let pos = vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (3.0, 7.0),
            (-5.0, 2.0),
            (1.0, -8.0),
            (6.0, 6.0),
            (-9.0, -4.0),
            (12.0, -3.0),
        ];
        let k = 50.0;
        let tree = QuadTree::build(&pos);
        for i in 0..pos.len() {
            let bh = tree.repulsion(i, pos[i], k, 0.0);
            let bf = brute_repulsion(&pos, i, k);
            assert!(
                (bh.0 - bf.0).abs() < 1e-6 && (bh.1 - bf.1).abs() < 1e-6,
                "node {i}: barnes-hut {bh:?} != brute force {bf:?}"
            );
        }
    }

    /// A synthetic ring-graph of `n` code nodes, for cap / label / scale tests.
    fn big_kg(n: usize) -> KnowledgeGraph {
        use codegraph_core::{Edge, FileType, GraphData, Node};
        let nodes = (0..n)
            .map(|i| Node {
                id: NodeId(format!("n{i}")),
                label: format!("Node_{i}"),
                file_type: FileType::Code,
                source_file: format!("src/m{}.rs", i % 16),
                source_location: Some("L1".into()),
                community: Some((i % 8) as u32),
                repo: None,
                extra: serde_json::Map::new(),
            })
            .collect();
        let links = (0..n)
            .map(|i| Edge {
                source: NodeId(format!("n{i}")),
                target: NodeId(format!("n{}", (i + 1) % n)),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                source_file: "src/m0.rs".into(),
                source_location: None,
                confidence_score: None,
                weight: 1.0,
                context: None,
                cross_repo: false,
                extra: serde_json::Map::new(),
            })
            .collect();
        KnowledgeGraph::from_graph_data(GraphData {
            directed: false,
            multigraph: false,
            graph: serde_json::Map::new(),
            nodes,
            links,
            hyperedges: vec![],
            built_at_commit: None,
        })
    }

    #[test]
    fn svg_has_nodes_edges_and_is_deterministic() {
        let kg = sample_kg();
        let a = to_svg_string(&kg);
        let b = to_svg_string(&kg);
        assert_eq!(a, b, "layout must be deterministic");
        assert!(a.contains("<svg"));
        assert!(a.contains("<circle"));
        assert!(a.contains("<line"));
        assert!(a.contains(">A</text>"));
    }

    #[test]
    fn layout_caps_at_max_nodes() {
        let (ids, pos) = layout(&big_kg(6000));
        assert_eq!(ids.len(), 5000, "lay out at most MAX_NODES");
        assert_eq!(pos.len(), 5000);
    }

    #[test]
    fn labels_suppressed_above_threshold() {
        // Below LABEL_MAX_NODES node labels render; above it only the legend's
        // per-kind text rows remain (less <text> overall), but nodes still draw.
        let small = to_svg_string(&big_kg(400));
        let big = to_svg_string(&big_kg(600));
        assert!(
            small.matches("<text").count() > big.matches("<text").count(),
            "small graph keeps node labels"
        );
        assert!(big.matches("<text").count() >= 1, "legend text remains");
        assert!(big.contains("<circle"), "but nodes still render");
    }

    #[test]
    fn large_layout_is_deterministic() {
        // 1500 > HYBRID_THRESHOLD (1000), exercises the Barnes-Hut path.
        let kg = big_kg(1500);
        assert_eq!(
            to_svg_string(&kg),
            to_svg_string(&kg),
            "barnes-hut layout must be reproducible at scale"
        );
    }
}
