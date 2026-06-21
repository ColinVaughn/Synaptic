//! Interactive 3D graph explorer (`graph-3d.html`). Embeds the graph as
//! node-link JSON and renders it with `3d-force-graph` (Three.js + d3-force-3d)
//! loaded from a version-pinned CDN. The browser runs the force simulation
//! live. Find/filter tools: search (fly-to + step matches), click-to-focus with
//! neighbor highlight + a details panel, relation toggles, and a degree slider
//! to declutter. Large graphs (node+edge count over a threshold) transparently
//! switch to a faster render path — edges as GL lines instead of cylinder
//! meshes, the regular code nodes collapsed into a single GPU-instanced mesh
//! (one draw call for the whole cloud, with custom raycast click + hover so
//! focus and tooltips still work), coarser node spheres, an off-screen warm-up,
//! a dimmer link haze, and a capped pixel ratio — so big scans stay interactive
//! without capping the scan itself.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::json;
use synaptic_graph::KnowledgeGraph;

use crate::common::{community_color, degrees, xml_escape};

/// Version-pinned CDN URL for the self-contained 3d-force-graph UMD bundle.
/// Assembled from pieces (the `@` kept separate) so the `pkg@version` segment
/// can't be mangled by an email-address obfuscator into "[email protected]".
const FORCE_GRAPH_CDN: &str = concat!(
    "https://unpkg.com/3d-force-graph",
    "@",
    "1.80.0",
    "/dist/3d-force-graph.min.js"
);

/// `three` ESM module (esm.sh) used ONLY to build the per-`asset_kind` node
/// geometries, loaded lazily via a dynamic `import()` so it's a progressive
/// enhancement: if it fails (or is unreachable) the asset nodes simply stay
/// default spheres and the graph still renders. We deliberately do NOT set a
/// global `window.THREE` — the 3d-force-graph UMD bundles its own (correct,
/// `>=0.179`) three and uses our global only if present, so hijacking it with a
/// mismatched version is what previously broke the viewer (`THREE.Timer` missing
/// on an older build). This version matches 3d-force-graph's bundled three, so
/// the meshes we hand back render in its scene. Assembled in pieces so the
/// `pkg@version` segment can't be mangled into "[email protected]" by an obfuscator.
const THREE_ESM: &str = concat!("https://esm.sh/three", "@", "0.179.1");

/// Serialize `value` to compact JSON and escape it for embedding inside a
/// single-quoted JavaScript string literal handed to `JSON.parse`. Escapes the
/// literal's own delimiters (`\` then `'`) and rewrites `</` to `<\/` so an
/// embedded `</script>` can't break out of the surrounding `<script>` block.
/// Order matters: `\` is doubled first so it doesn't double the backslash the
/// `</` rewrite adds (that one is for the JS layer; `JSON.parse` never sees it).
fn json_string_literal(value: &[serde_json::Value]) -> String {
    serde_json::to_string(value)
        .expect("serde_json::Value re-serializes")
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace("</", "<\\/")
}

/// Render the graph as a standalone interactive 3D HTML document.
pub fn to_force3d_html(kg: &KnowledgeGraph) -> String {
    let deg = degrees(kg);
    let max_deg = deg.values().copied().max().unwrap_or(1).max(1);

    let repos = crate::common::repo_index(kg);
    let federated = !repos.is_empty();
    let nodes: Vec<_> = kg
        .nodes()
        .map(|n| {
            // The real NodeKind (table/column/function/...) drives shape, the
            // "color by kind" mode, the legend, and the kind filters, so the SQL
            // and cross-language layers are visible — not just "code".
            let asset_kind = n.extra.get("asset_kind").and_then(|v| v.as_str());
            let kind = crate::common::visual_kind(n);
            let mut v = json!({
                "id": n.id.0,
                "name": n.label,
                "kind": kind,
                "kindColor": crate::common::kind_color(kind),
                "assetKind": asset_kind,
                "val": deg.get(&n.id).copied().unwrap_or(1).max(1),
                "community": n.community.unwrap_or(0),
                "color": community_color(n.community.unwrap_or(0) as usize),
                "file": n.source_file,
            });
            // SQL facts for the details panel (only the keys that are set).
            let mut sql = serde_json::Map::new();
            for k in ["dialect", "data_type", "fk_target"] {
                if let Some(s) = n.extra.get(k).and_then(|x| x.as_str()) {
                    sql.insert(k.into(), json!(s));
                }
            }
            for k in ["pk", "rls_enabled", "rls_forced", "security_invoker"] {
                if n.extra.get(k).and_then(|x| x.as_bool()) == Some(true) {
                    sql.insert(k.into(), json!(true));
                }
            }
            if !sql.is_empty() {
                v.as_object_mut()
                    .expect("json object")
                    .insert("sql".into(), json!(sql));
            }
            // Federation fields are added ONLY for federated graphs, so single-repo
            // output (and its generation cost) stays identical to a non-federated
            // build.
            if federated {
                let repo_color = n
                    .repo
                    .as_deref()
                    .and_then(|t| repos.get(t))
                    .map(|&i| crate::common::repo_color(i))
                    .unwrap_or("#888888");
                let o = v.as_object_mut().expect("json object");
                o.insert("repo".into(), json!(n.repo));
                o.insert("repoColor".into(), json!(repo_color));
                o.insert(
                    "external".into(),
                    json!(crate::common::is_external_package(n)),
                );
            }
            v
        })
        .collect();
    let links: Vec<_> = kg
        .edges()
        .map(|e| {
            // Color by relation so SQL structure and code->SQL bridges read at a
            // glance; `bridge` powers the "bridges only" cross-language filter.
            let color = crate::common::relation_color(&e.relation);
            let mut v = json!({
                "source": e.source.0,
                "target": e.target.0,
                "relation": e.relation,
                "color": color,
                "bridge": crate::common::is_bridge_relation(&e.relation),
            });
            if federated && e.cross_repo {
                v.as_object_mut()
                    .expect("json object")
                    .insert("crossRepo".into(), json!(true));
            }
            v
        })
        .collect();

    // Embed the payload as `JSON.parse('…')` rather than an inline JS object/array
    // literal: V8 parses a large JSON *string* substantially faster than the
    // equivalent literal, which matters for the big scans the PERF path targets.
    let nodes_json = json_string_literal(&nodes);
    let links_json = json_string_literal(&links);

    // One filter checkbox per relation actually present, sorted for determinism.
    let mut rels: BTreeSet<&str> = BTreeSet::new();
    for e in kg.edges() {
        rels.insert(e.relation.as_str());
    }
    let relation_checkboxes: String = rels
        .iter()
        .map(|r| {
            let e = xml_escape(r);
            format!("<label><input type=\"checkbox\" data-relation=\"{e}\" checked>{e}</label>")
        })
        .collect();

    // Kinds present drive the kind filters (= a schema/layer view) and the legend;
    // a color swatch in each row doubles as the color-by-kind key.
    let mut kinds: BTreeSet<&str> = BTreeSet::new();
    let mut has_columns = false;
    for n in kg.nodes() {
        let k = crate::common::visual_kind(n);
        kinds.insert(k);
        has_columns |= k == "column";
    }
    let kind_checkboxes: String = kinds
        .iter()
        .map(|k| {
            let e = xml_escape(k);
            format!(
                "<label><input type=\"checkbox\" data-kind=\"{e}\" checked><span class=\"sw\" style=\"background:{}\"></span>{e}</label>",
                crate::common::kind_color(k)
            )
        })
        .collect();
    let has_bridges = kg
        .edges()
        .any(|e| crate::common::is_bridge_relation(&e.relation));
    let colorby_options = if federated {
        "<option value=\"community\">community</option><option value=\"kind\">kind</option><option value=\"repo\">repo</option>"
    } else {
        "<option value=\"community\">community</option><option value=\"kind\">kind</option>"
    };
    let column_toggle = if has_columns {
        "<label class=\"asset-toggle\"><input type=\"checkbox\" id=\"show-columns\" checked> Show SQL columns</label>"
    } else {
        ""
    };
    let bridge_toggle = if has_bridges {
        "<label class=\"asset-toggle\"><input type=\"checkbox\" id=\"bridges-only\"> Code\u{2194}SQL bridges only</label>"
    } else {
        ""
    };

    // Federation controls (color-by-repo toggle, cross-repo filter, repo legend),
    // only when the graph carries repo tags.
    let repo_controls = if repos.is_empty() {
        String::new()
    } else {
        let mut sw = String::new();
        let mut entries: Vec<(&String, &usize)> = repos.iter().collect();
        entries.sort_by_key(|(_, i)| **i);
        for (tag, i) in entries {
            sw.push_str(&format!(
                "<div><span style=\"display:inline-block;width:10px;height:10px;border-radius:2px;background:{}\"></span> {}</div>",
                crate::common::repo_color(*i),
                xml_escape(tag)
            ));
        }
        format!(
            "<div class=\"sec\">Repos</div>\
             <label class=\"asset-toggle\"><input type=\"checkbox\" id=\"cross-only\"> cross-repo edges only</label>\
             <div id=\"repolegend\" class=\"muted\">{sw}</div>"
        )
    };

    TEMPLATE
        .replace("__THREE_ESM__", THREE_ESM)
        .replace("__CDN__", FORCE_GRAPH_CDN)
        .replace("__NODE_COUNT__", &kg.node_count().to_string())
        .replace("__EDGE_COUNT__", &kg.edge_count().to_string())
        .replace("__MAX_DEG__", &max_deg.to_string())
        .replace("__RELATIONS__", &relation_checkboxes)
        .replace("__KINDS__", &kind_checkboxes)
        .replace("__COLORBY_OPTIONS__", colorby_options)
        .replace("__COLUMN_TOGGLE__", column_toggle)
        .replace("__BRIDGE_TOGGLE__", bridge_toggle)
        .replace("__REPO_CONTROLS__", &repo_controls)
        .replace("__CROSS_COLOR__", crate::common::CROSS_REPO_COLOR)
        .replace("__NODES__", &nodes_json)
        .replace("__LINKS__", &links_json)
}

/// HTML/CSS/JS shell with `__TOKEN__` placeholders filled by `to_force3d_html`.
/// A token-replaced raw string (not `format!`) so the brace-heavy JS stays
/// readable. Tokens are distinct and never substrings of each other.
const TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Synaptic 3D</title>
<script src="__CDN__"></script>
<style>
  html, body { margin: 0; height: 100%; background: #1a1a2e; color: #eee; font-family: system-ui, sans-serif; overflow: hidden; }
  #graph { position: absolute; inset: 0; }
  #panel { position: absolute; top: 12px; left: 12px; width: 232px; background: rgba(20,20,40,0.88);
           border: 1px solid #333; border-radius: 8px; padding: 12px; z-index: 10; font-size: 13px; }
  #panel .title { font-weight: 700; font-size: 15px; }
  .muted { opacity: 0.65; font-size: 12px; }
  #search { width: 100%; box-sizing: border-box; margin-top: 8px; padding: 5px 8px;
            border-radius: 4px; border: 1px solid #444; background: #222; color: #eee; }
  .sec { margin-top: 12px; font-weight: 600; opacity: 0.9; }
  #relations, #kinds { display: flex; flex-direction: column; gap: 2px; max-height: 148px; overflow: auto; margin-top: 4px; }
  #relations label, #kinds label { display: flex; gap: 6px; align-items: center; font-size: 12px; cursor: pointer; }
  #kinds .sw { display: inline-block; width: 9px; height: 9px; border-radius: 2px; flex: 0 0 auto; }
  #colorby { background: #2a2a44; color: #eee; border: 1px solid #555; border-radius: 4px; }
  #degree, #spread { width: 100%; margin-top: 6px; }
  .asset-toggle { display: flex; gap: 6px; align-items: center; margin-top: 10px; font-size: 12px; cursor: pointer; }
  #legend { margin-top: 6px; line-height: 1.7; }
  #reset { margin-top: 10px; padding: 4px 10px; border-radius: 4px; border: 1px solid #555; background: #2a2a44; color: #eee; cursor: pointer; }
  #details { position: absolute; top: 12px; right: 12px; width: 264px; max-height: 82vh; overflow: auto;
             background: rgba(20,20,40,0.92); border: 1px solid #333; border-radius: 8px; padding: 12px; z-index: 10; display: none; }
  #details h3 { margin: 0 0 4px; font-size: 15px; word-break: break-all; }
  #details .nb { display: flex; flex-direction: column; gap: 2px; margin-top: 6px; }
  #details .nb a { color: #8bd3ff; cursor: pointer; font-size: 12px; text-decoration: none; }
  #details .nb a:hover { text-decoration: underline; }
  #tip { position: fixed; z-index: 20; pointer-events: none; display: none; max-width: 340px;
         white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
         background: rgba(20,20,40,0.95); border: 1px solid #444; border-radius: 4px;
         padding: 3px 7px; font-size: 12px; color: #eee; }
</style>
</head>
<body>
<div id="panel">
  <div class="title">Synaptic 3D</div>
  <div class="muted">__NODE_COUNT__ nodes &middot; __EDGE_COUNT__ edges</div>
  <input id="search" type="text" placeholder="search label&hellip;" autocomplete="off">
  <div id="results" class="muted"></div>
  <label class="asset-toggle">Color by <select id="colorby">__COLORBY_OPTIONS__</select></label>
  __BRIDGE_TOGGLE__
  <div class="sec">Kinds</div>
  <div id="kinds">__KINDS__</div>
  __COLUMN_TOGGLE__
  <div class="sec">Relations</div>
  <div id="relations">__RELATIONS__</div>
  <div class="sec">Min connections: <span id="degval">0</span></div>
  <input id="degree" type="range" min="0" max="__MAX_DEG__" value="0">
  <div class="sec">Spread: <span id="spreadval">1x</span></div>
  <input id="spread" type="range" min="1" max="10" value="1" step="0.5">
  <label class="asset-toggle"><input type="checkbox" id="show-assets" checked> Show assets (css/json/img)</label>
  <div id="legend" class="muted">
    &#9670; table &middot; &bull; column &middot; &#9650; view &middot; &#9632; index/class &middot; &#11042; proc &middot; &#9733; trigger &middot; &#9679; code
  </div>
  __REPO_CONTROLS__
  <div><button id="reset">reset view</button></div>
  <div class="muted" style="margin-top:8px">click a node to focus &middot; enter steps matches &middot; drag to rotate</div>
</div>
<div id="details"></div>
<div id="tip"></div>
<div id="graph"></div>
<script>
  const data = { nodes: JSON.parse('__NODES__'), links: JSON.parse('__LINKS__') };
  const byId = new Map(data.nodes.map(n => [n.id, n]));
  const adjNodes = new Map(data.nodes.map(n => [n.id, new Set()]));
  const adjLinks = new Map(data.nodes.map(n => [n.id, new Set()]));
  data.links.forEach(l => {
    const s = (typeof l.source === 'object') ? l.source.id : l.source;
    const t = (typeof l.target === 'object') ? l.target.id : l.target;
    if (byId.has(s) && byId.has(t)) {
      adjNodes.get(s).add(byId.get(t)); adjNodes.get(t).add(byId.get(s));
      adjLinks.get(s).add(l); adjLinks.get(t).add(l);
    }
  });

  let hlNodes = new Set(), hlLinks = new Set(), selected = null, minDeg = 0, showAssets = true;
  let colorBy = 'community', crossOnly = false, showColumns = true, bridgesOnly = false;
  const relEnabled = {}, kindEnabled = {};
  const CROSS = '__CROSS_COLOR__';
  // SQL schema objects get a distinct 3D mesh; columns and code stay spheres (of
  // which there can be tens of thousands — the structural objects are few).
  const STRUCT_KINDS = new Set(['table','view','index','procedure','trigger','policy','role']);
  const meshKind = n => n.assetKind || (STRUCT_KINDS.has(n.kind) ? n.kind : null);
  const baseColor = n => colorBy === 'kind' ? (n.kindColor || '#b0bec5')
                       : colorBy === 'repo' ? (n.repoColor || '#888888') : n.color;
  // Shared node-visibility predicate: the degree slider and the show-assets toggle
  // both hide nodes, and search must honour the same rules so it never flies the
  // camera to (and "highlights") a node that's currently filtered out of view.
  function nodeVisible(n) { return (showAssets || !n.assetKind) && (showColumns || n.kind !== 'column') && kindEnabled[n.kind] !== false && n.val >= minDeg; }

  // Non-code (asset) nodes render as a distinct shape per kind, taking their
  // color from baseColor (community, or repo under the color-by toggle). THREE is
  // loaded lazily (see the dynamic import at the end) as a progressive
  // enhancement: until/unless it arrives, assetMesh returns null and the nodes are
  // default spheres, so the graph always renders. Geometries are cached per kind
  // and materials per color, so even thousands of asset nodes allocate only a
  // handful of GPU objects.
  let THREE = null;
  const _geo = {}, _mat = {};
  function shapeGeo(kind) {
    switch (kind) {
      // assets
      case 'stylesheet': return new THREE.BoxGeometry(7, 7, 7);
      case 'data':       return new THREE.OctahedronGeometry(5);
      case 'image':      return new THREE.TetrahedronGeometry(6.5);
      case 'font':       return new THREE.CylinderGeometry(4, 4, 8, 12);
      case 'media':      return new THREE.TorusGeometry(4, 1.5, 8, 16);
      // SQL schema objects
      case 'table':      return new THREE.OctahedronGeometry(6);
      case 'view':       return new THREE.ConeGeometry(5, 9, 4);
      case 'index':      return new THREE.BoxGeometry(6, 6, 6);
      case 'procedure':  return new THREE.CylinderGeometry(4.5, 4.5, 8, 6);
      case 'trigger':    return new THREE.TorusKnotGeometry(3, 1, 48, 6);
      case 'policy':     return new THREE.ConeGeometry(5, 9, 3);
      case 'role':       return new THREE.DodecahedronGeometry(5);
      default:           return new THREE.DodecahedronGeometry(5);
    }
  }
  // Color for an asset/external custom mesh. The library applies its nodeColor
  // (highlight white / dim gray / base) only to its OWN generated spheres, NOT to
  // custom nodeThreeObject meshes — so we mirror that logic here and repaint the
  // meshes ourselves (paintCustom). Routing through baseColor also makes assets
  // follow the color-by-repo toggle like every other node, instead of being
  // pinned to their community color.
  function customColor(n) {
    if (hlNodes.size) return hlNodes.has(n) ? (n === selected ? '#ffffff' : baseColor(n)) : '#2b2b3c';
    return baseColor(n);
  }
  // Material for a custom mesh, cached so nodes sharing a (transparency, color)
  // pair share one material — the dim ('#2b2b3c') and selected ('#ffffff') states
  // collapse to one shared material each, like the per-color base ones, so even
  // thousands of asset nodes still allocate only a handful of materials.
  function customMat(n) {
    const col = customColor(n), ext = !!n.external, key = (ext ? '__ext' : '') + col;
    return _mat[key] || (_mat[key] = new THREE.MeshLambertMaterial(
      ext ? { color: col, transparent: true, opacity: 0.35 } : { color: col }));
  }
  function assetMesh(n) {
    if (!THREE) return null;
    const k = meshKind(n) || 'asset';
    const g = _geo[k] || (_geo[k] = shapeGeo(k));
    const mesh = new THREE.Mesh(g, customMat(n));
    mesh.userData.node = n; // tag for the PERF-mode custom raycast picker
    n.__mesh = mesh;        // so paintCustom() can recolor it on highlight changes
    return mesh;
  }
  // External-package stubs render as a translucent sphere — the 3D analog of the
  // 2D dashed ring ("not our code"). Sized by degree like default nodes.
  function externalMesh(n) {
    if (!THREE) return null;
    const g = _geo['__ext'] || (_geo['__ext'] = new THREE.SphereGeometry(1, 12, 12));
    const mesh = new THREE.Mesh(g, customMat(n));
    mesh.scale.setScalar(Math.cbrt(n.val) * 3 + 2);
    mesh.userData.node = n; // tag for the PERF-mode custom raycast picker
    n.__mesh = mesh;        // so paintCustom() can recolor it on highlight changes
    return mesh;
  }
  // Asset/external nodes render as custom meshes the library won't recolor, so we
  // repaint them ourselves on every highlight/color change (focus, search, the
  // color-by toggle, reset). No-op until THREE has built the meshes; before that
  // the nodes are library spheres which the library's nodeColor already dims.
  const customNodes = data.nodes.filter(n => meshKind(n) || n.external);
  function paintCustom() {
    if (!THREE) return;
    for (const n of customNodes) if (n.__mesh) n.__mesh.material = customMat(n);
  }
  const esc = s => String(s == null ? '' : s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const ends = l => [ (typeof l.source === 'object') ? l.source : byId.get(l.source),
                      (typeof l.target === 'object') ? l.target : byId.get(l.target) ];

  // --- Adaptive performance scaling for large graphs ------------------------
  // A link drawn with a *non-zero* width becomes an individual ThreeJS cylinder
  // mesh (one draw call each); at width 0 it is a cheap shared GL line. So on a
  // big graph the edges alone cost thousands of cylinder draws every frame — the
  // dominant steady-state (rotation) cost. Above a size threshold we therefore
  // drop normal edges to lines, coarsen the node spheres, pre-settle the layout
  // off-screen (warmupTicks) so it doesn't animate from a cold explosion, and
  // cap the device-pixel-ratio (below) so HiDPI panels stop shading ~4x pixels.
  // Small graphs (PERF=false) keep cylinder edges, full sphere resolution, and a
  // cold animated layout; the only render change they share is the DPR cap (set
  // to 2 below, vs PERF's 1.5).
  const PERF = (data.nodes.length + data.links.length) > 6000;
  // Bound the synchronous warm-up so a very large scan can't freeze the tab on
  // load: the tick budget shrinks as the graph grows (more nodes => fewer ticks).
  const WARMUP = PERF ? Math.min(60, Math.max(8, Math.round(2e5 / data.nodes.length))) : 0;

  const Graph = ForceGraph3D()(document.getElementById('graph'))
    .graphData(data)
    .backgroundColor('#1a1a2e')
    .nodeResolution(PERF ? 6 : 8)
    .warmupTicks(WARMUP)
    // Bound the on-screen simulation so it settles and STOPS, instead of ticking
    // for the default 15s. Each on-screen tick on a large graph is a 50-100ms
    // requestAnimationFrame handler — the source of the console-spamming
    // "[Violation] 'requestAnimationFrame' handler took Nms". A faster alpha /
    // velocity decay settles in fewer ticks; cooldownTicks/Time cap how long it
    // runs. Once the engine stops (onEngineStop), rAF only renders the (instanced)
    // scene, which is cheap — so the violations end. Small graphs keep the
    // defaults (cheap ticks, no spam).
    .cooldownTicks(PERF ? 80 : Infinity)
    .cooldownTime(PERF ? 6000 : 15000)
    .d3AlphaDecay(PERF ? 0.05 : 0.0228)
    .d3VelocityDecay(PERF ? 0.5 : 0.4)
    .nodeLabel(n => n.name + ' (' + n.kind + ')')
    .nodeVal('val')
    .nodeColor(n => hlNodes.size ? (hlNodes.has(n) ? (n === selected ? '#ffffff' : baseColor(n)) : '#2b2b3c') : baseColor(n))
    .nodeOpacity(0.92)
    .nodeThreeObject(n => meshKind(n) ? assetMesh(n) : (n.external ? externalMesh(n) : ((PERF && THREE) ? new THREE.Object3D() : null)))
    .linkColor(l => hlLinks.has(l) ? '#ffffff' : (l.crossRepo ? CROSS : l.color))
    .linkWidth(l => hlLinks.has(l) ? 1.5 : (l.crossRepo ? 1.2 : (l.bridge ? 0.9 : (PERF ? 0 : 0.4))))
    // GL lines render brighter (constant 1px) than the thin cylinders they
    // replace, so on a dense graph the majority "extracted" (green) edges wash
    // everything out. A lower opacity in PERF mode keeps the link mesh readable
    // and lets the nodes / other-confidence edges show through.
    .linkOpacity(PERF ? 0.15 : 0.3)
    .nodeVisibility(nodeVisible)
    .linkVisibility(l => { const [s, t] = ends(l); return relEnabled[l.relation] !== false && (!crossOnly || l.crossRepo) && (!bridgesOnly || l.bridge) && nodeVisible(s) && nodeVisible(t); })
    .onNodeClick(n => focusNode(n))
    .onBackgroundClick(() => clearFocus())
    // Keep the instanced node cloud (PERF mode) in sync with the live layout,
    // then leave it frozen once the engine stops — no per-frame work at rest.
    .onEngineTick(() => { if (PERF) updateInstances(); })
    .onEngineStop(() => { if (PERF) updateInstances(); });

  // Cap the device-pixel-ratio: on a retina/4K panel an uncapped renderer shades
  // ~4x the fragments every frame, the cheapest FPS win there is. Big graphs get
  // a tighter cap; small graphs stay crisp (still capped at 2 to spare 3x phones).
  try {
    const _r = Graph.renderer && Graph.renderer();
    if (_r && _r.setPixelRatio) _r.setPixelRatio(Math.min(window.devicePixelRatio || 1, PERF ? 1.5 : 2));
  } catch (e) {}

  function applyHighlight() { Graph.nodeColor(Graph.nodeColor()).linkColor(Graph.linkColor()).linkWidth(Graph.linkWidth()); paintCustom(); if (PERF) paintInstances(); }
  function applyFilters() { Graph.nodeVisibility(Graph.nodeVisibility()).linkVisibility(Graph.linkVisibility()); if (PERF) updateInstances(); }

  // --- GPU node instancing (PERF mode) --------------------------------------
  // Once edges are GL lines, the last big per-frame cost is one sphere *mesh*
  // per regular code node (thousands of draw calls). We collapse them into a
  // single THREE.InstancedMesh — one draw call for the whole node cloud — built
  // lazily when THREE arrives (same progressive-enhancement import as the asset
  // shapes; if it never loads, regular nodes stay library spheres and picking
  // stays on the library). Asset/external nodes keep their bespoke meshes (a
  // small minority). Instanced nodes are not library objects, so the library
  // can't hover/pick them; once instancing is live we disable its pointer layer
  // and raycast the instanced mesh (+ the asset/external meshes) ourselves for
  // both click-to-focus and hover tooltips. Positions/colors are synced from
  // the live layout each tick and frozen on stop, so steady-state rotation does
  // no extra per-frame work.
  let instMesh = null, instNodes = [], pickObjs = [], _ray = null, _hovEv = null, _hovPending = false, _downPt = null;
  const NODE_REL = 4; // 3d-force-graph's nodeRelSize default: radius = cbrt(val) * 4
  function instColor(n) {
    if (hlNodes.size) return hlNodes.has(n) ? (n === selected ? '#ffffff' : baseColor(n)) : '#2b2b3c';
    return baseColor(n);
  }
  function paintInstances() {
    if (!instMesh) return;
    const c = new THREE.Color();
    for (let i = 0; i < instNodes.length; i++) instMesh.setColorAt(i, c.set(instColor(instNodes[i])));
    if (instMesh.instanceColor) instMesh.instanceColor.needsUpdate = true;
  }
  function updateInstances() {
    if (!instMesh) return;
    const m = new THREE.Matrix4(), q = new THREE.Quaternion(), p = new THREE.Vector3(), s = new THREE.Vector3();
    for (let i = 0; i < instNodes.length; i++) {
      const n = instNodes[i];
      // The degree slider hides low-connection nodes by scaling them to nothing.
      const k = n.val >= minDeg ? Math.cbrt(Math.max(n.val, 1)) * NODE_REL : 0;
      p.set(n.x || 0, n.y || 0, n.z || 0); s.set(k, k, k);
      instMesh.setMatrixAt(i, m.compose(p, q, s));
    }
    instMesh.instanceMatrix.needsUpdate = true;
  }
  function buildInstances() {
    if (!THREE || instMesh) return;
    instNodes = data.nodes.filter(n => !meshKind(n) && !n.external);
    if (!instNodes.length) return;
    const r = PERF ? 6 : 8;
    const geo = new THREE.SphereGeometry(1, r, r);
    // Opaque on purpose: cheaper than alpha-blending thousands of instances and
    // makes the nodes read clearly on top of the translucent link haze.
    const mat = new THREE.MeshLambertMaterial({});
    instMesh = new THREE.InstancedMesh(geo, mat, instNodes.length);
    instMesh.instanceMatrix.setUsage(THREE.DynamicDrawUsage);
    instMesh.frustumCulled = false; // one object for the whole cloud; per-instance cull buys nothing
    // Raycast picking: three caches ONE bounding sphere for the whole instanced
    // mesh and early-outs if the ray misses it. It's computed once, but the
    // layout keeps expanding as the sim settles, so nodes drifting outside the
    // stale sphere become unclickable (you can only pick a shrinking central
    // region). Pin a huge sphere so the early-out never rejects and every
    // instance is tested precisely — picking stays exact at any layout spread.
    instMesh.boundingSphere = new THREE.Sphere(new THREE.Vector3(), 1e9);
    Graph.scene().add(instMesh);
    updateInstances(); paintInstances();
    // The library can't pick instanced nodes: take over picking/hover and drop
    // its (now redundant) pointer raycasting. Camera controls are unaffected.
    Graph.enablePointerInteraction(false);
    const dom = Graph.renderer().domElement;
    // The camera controls preventDefault() on pointerdown, which suppresses the
    // native 'click' event — so detect a click ourselves: a pointerup landing
    // near where pointerdown started (a tap), not a rotate-drag. pointerup is on
    // window in case the controls pointer-capture the canvas mid-gesture.
    dom.addEventListener('pointerdown', ev => { _downPt = { x: ev.clientX, y: ev.clientY }; });
    window.addEventListener('pointerup', ev => {
      if (!_downPt) return;
      const moved = Math.hypot(ev.clientX - _downPt.x, ev.clientY - _downPt.y);
      _downPt = null;
      if (moved < 6) onCanvasClick(ev);
    });
    dom.addEventListener('pointermove', onCanvasMove);
    dom.addEventListener('pointerleave', onCanvasLeave);
  }
  function refreshPickObjs() {
    // Cache the asset/external meshes (a small set) so picking/hover never has to
    // traverse the whole scene — which includes thousands of empty placeholder
    // objects — on every frame.
    pickObjs = [];
    Graph.scene().traverse(o => { if (o.userData && o.userData.node) pickObjs.push(o); });
  }
  function pickNodeAt(ev) {
    if (!THREE) return null;
    const dom = Graph.renderer().domElement, rect = dom.getBoundingClientRect();
    _ray = _ray || new THREE.Raycaster();
    _ray.setFromCamera({ x: ((ev.clientX - rect.left) / rect.width) * 2 - 1,
                         y: -((ev.clientY - rect.top) / rect.height) * 2 + 1 }, Graph.camera());
    let best = null, bestDist = Infinity;
    if (instMesh) {
      const hits = _ray.intersectObject(instMesh, false);
      if (hits.length) { const n = instNodes[hits[0].instanceId]; if (n && n.val >= minDeg) { best = n; bestDist = hits[0].distance; } }
    }
    // Asset/external meshes carry userData.node — raycast just those, not the lines.
    if (pickObjs.length) for (const h of _ray.intersectObjects(pickObjs, true)) {
      if (h.distance >= bestDist) break;
      let o = h.object; while (o && !(o.userData && o.userData.node)) o = o.parent;
      if (o) { best = o.userData.node; break; }
    }
    return best;
  }
  function onCanvasClick(ev) { const n = pickNodeAt(ev); if (n) focusNode(n); else clearFocus(); }
  // Hover tooltip (replaces the library's, which needs its pointer layer): one
  // hit-test per animation frame, only while the cursor moves, so it's free at
  // rest and a single ~5k-instance ray test (microseconds) while moving.
  function doHover() {
    _hovPending = false;
    const tip = document.getElementById('tip'), dom = Graph.renderer().domElement;
    const n = _hovEv ? pickNodeAt(_hovEv) : null;
    if (n) {
      tip.textContent = n.name + ' (' + n.kind + ')';
      tip.style.left = (_hovEv.clientX + 14) + 'px'; tip.style.top = (_hovEv.clientY + 14) + 'px';
      tip.style.display = 'block'; dom.style.cursor = 'pointer';
    } else { tip.style.display = 'none'; dom.style.cursor = ''; }
  }
  function onCanvasMove(ev) { _hovEv = ev; if (!_hovPending) { _hovPending = true; requestAnimationFrame(doHover); } }
  function onCanvasLeave() { document.getElementById('tip').style.display = 'none'; }

  function focusNode(node) {
    selected = node;
    hlNodes = new Set([node]); (adjNodes.get(node.id) || []).forEach(n => hlNodes.add(n));
    hlLinks = new Set(adjLinks.get(node.id) || []);
    const dist = 140, h = Math.hypot(node.x || 0.1, node.y || 0.1, node.z || 0.1), r = 1 + dist / (h || 1);
    Graph.cameraPosition({ x: (node.x || 0) * r, y: (node.y || 0) * r, z: (node.z || 0) * r }, node, 1200);
    applyHighlight(); showDetails(node);
  }
  function clearFocus() {
    selected = null; hlNodes = new Set(); hlLinks = new Set();
    applyHighlight(); document.getElementById('details').style.display = 'none';
  }

  function showDetails(node) {
    const d = document.getElementById('details');
    const nb = [...(adjNodes.get(node.id) || [])].sort((a, b) => b.val - a.val);
    const sql = node.sql ? '<div class="muted" style="margin-top:4px">'
      + Object.entries(node.sql).map(([k, v]) => v === true ? k : (k + ': ' + esc(v))).join(' &middot; ')
      + '</div>' : '';
    d.innerHTML = '<h3>' + esc(node.name) + '</h3>'
      + '<div class="muted">' + esc(node.kind) + ' &middot; ' + node.val + ' links &middot; community ' + node.community + '</div>'
      + '<div class="muted">' + esc(node.file) + '</div>'
      + sql
      + '<div class="sec">Connected (' + nb.length + ')</div>'
      + '<div class="nb">' + nb.slice(0, 50).map(n => '<a data-id="' + esc(n.id) + '">' + esc(n.name) + '</a>').join('') + '</div>';
    d.style.display = 'block';
    d.querySelectorAll('a[data-id]').forEach(a => a.onclick = () => { const n = byId.get(a.getAttribute('data-id')); if (n) focusNode(n); });
  }

  const search = document.getElementById('search'), results = document.getElementById('results');
  let matches = [], mi = 0;
  search.addEventListener('input', () => {
    const q = search.value.trim().toLowerCase();
    matches = q ? data.nodes.filter(n => nodeVisible(n) && (n.name || '').toLowerCase().includes(q)) : [];
    mi = 0;
    results.textContent = q ? (matches.length + ' match' + (matches.length === 1 ? '' : 'es')) : '';
    if (matches.length) focusNode(matches[0]); else clearFocus();
  });
  search.addEventListener('keydown', e => {
    if (e.key === 'Enter' && matches.length) { mi = (mi + 1) % matches.length; focusNode(matches[mi]); results.textContent = (mi + 1) + '/' + matches.length; }
  });

  document.querySelectorAll('input[data-relation]').forEach(cb => {
    relEnabled[cb.getAttribute('data-relation')] = cb.checked;
    cb.addEventListener('change', () => { relEnabled[cb.getAttribute('data-relation')] = cb.checked; applyFilters(); });
  });
  // Kind filters double as a schema/layer view: uncheck everything but table +
  // column to see just the schema, or but the code kinds to see just the code.
  document.querySelectorAll('input[data-kind]').forEach(cb => {
    kindEnabled[cb.getAttribute('data-kind')] = cb.checked;
    cb.addEventListener('change', () => { kindEnabled[cb.getAttribute('data-kind')] = cb.checked; applyFilters(); });
  });

  const degree = document.getElementById('degree'), degval = document.getElementById('degval');
  degree.addEventListener('input', () => { minDeg = +degree.value; degval.textContent = minDeg; applyFilters(); });

  // Spread slider: scale the many-body (charge) repulsion and the link distance so
  // a dense central clump expands outward, then reheat the live sim to re-space.
  // d3-force-3d defaults are charge strength -30 and link distance 30, so 1x leaves
  // the layout untouched. This is layout-only: no node is hidden (that's what the
  // degree slider does). Works in PERF mode too — onEngineTick re-syncs the
  // instanced node cloud as the reheated layout settles.
  const CHARGE_BASE = -30, LINK_BASE = 30;
  let spreadFactor = 1;
  const spread = document.getElementById('spread'), spreadval = document.getElementById('spreadval');
  function applySpread() {
    spreadFactor = +spread.value;
    spreadval.textContent = spreadFactor + 'x';
    const ch = Graph.d3Force('charge'); if (ch && ch.strength) ch.strength(CHARGE_BASE * spreadFactor);
    const lk = Graph.d3Force('link'); if (lk && lk.distance) lk.distance(LINK_BASE * spreadFactor);
    Graph.d3ReheatSimulation();
  }
  spread.addEventListener('input', applySpread);

  const showAssetsCb = document.getElementById('show-assets');
  showAssetsCb.addEventListener('change', () => { showAssets = showAssetsCb.checked; applyFilters(); });
  const showColsCb = document.getElementById('show-columns');
  if (showColsCb) showColsCb.addEventListener('change', () => { showColumns = showColsCb.checked; applyFilters(); });
  const bridgesCb = document.getElementById('bridges-only');
  if (bridgesCb) bridgesCb.addEventListener('change', () => { bridgesOnly = bridgesCb.checked; applyFilters(); });

  // Federation controls (present only when the graph carries repo tags).
  const colorby = document.getElementById('colorby');
  if (colorby) colorby.addEventListener('change', () => { colorBy = colorby.value; Graph.nodeColor(Graph.nodeColor()).nodeThreeObject(Graph.nodeThreeObject()); if (PERF) { paintInstances(); refreshPickObjs(); } });
  const crossCb = document.getElementById('cross-only');
  if (crossCb) crossCb.addEventListener('change', () => { crossOnly = crossCb.checked; applyFilters(); });

  document.getElementById('reset').addEventListener('click', () => {
    search.value = ''; results.textContent = ''; matches = [];
    degree.value = 0; minDeg = 0; degval.textContent = '0';
    if (spreadFactor !== 1) { spread.value = 1; applySpread(); } // only reheat if it was changed
    showAssets = true; showAssetsCb.checked = true;
    showColumns = true; if (showColsCb) showColsCb.checked = true;
    bridgesOnly = false; if (bridgesCb) bridgesCb.checked = false;
    document.querySelectorAll('input[data-relation]').forEach(cb => { cb.checked = true; relEnabled[cb.getAttribute('data-relation')] = true; });
    document.querySelectorAll('input[data-kind]').forEach(cb => { cb.checked = true; kindEnabled[cb.getAttribute('data-kind')] = true; });
    if (colorby) { colorby.value = 'community'; colorBy = 'community'; }
    if (crossCb) { crossCb.checked = false; crossOnly = false; }
    Graph.nodeColor(Graph.nodeColor()).nodeThreeObject(Graph.nodeThreeObject());
    if (PERF) refreshPickObjs();
    clearFocus(); applyFilters(); // applyFilters/clearFocus repaint+rescale the instanced cloud (PERF)
  });

  // Progressive enhancement: pull in a matching THREE (NOT as window.THREE, so we
  // don't override the bundled one 3d-force-graph uses) to give asset nodes a
  // per-kind shape and — in PERF mode — to back the instanced node cloud. If this
  // import fails for any reason, asset nodes stay spheres, regular nodes stay
  // library spheres (with library picking), and everything else is unaffected.
  // Once it lands, build the InstancedMesh (PERF) and re-trigger nodeThreeObject
  // so regular nodes become empty placeholders and assets pick up their shape.
  if (PERF || data.nodes.some(n => meshKind(n) || n.external)) {
    import('__THREE_ESM__')
      .then(m => {
        THREE = m;
        if (PERF) buildInstances();
        Graph.nodeThreeObject(Graph.nodeThreeObject());
        if (PERF) refreshPickObjs(); // cache asset/external meshes built by the retrigger
      })
      .catch(() => {});
  }
</script>
</body>
</html>
"##;

/// Write `graph-3d.html`.
pub fn to_force3d(kg: &KnowledgeGraph, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, to_force3d_html(kg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{kg_with_asset, kg_with_label, sample_kg};

    #[test]
    fn force3d_html_embeds_graph_with_pinned_cdn() {
        let html = to_force3d_html(&sample_kg());
        assert!(html.contains("1.80.0"), "pinned CDN version");
        assert!(
            !html.contains("email protected"),
            "CDN url must not be email-obfuscated (the @version looks like an email)"
        );
        assert!(
            html.contains(FORCE_GRAPH_CDN),
            "loads the 3d-force-graph bundle"
        );
        assert!(html.contains("ForceGraph3D"), "initializes the 3D graph");
        assert!(html.contains("\"A\""), "embeds node labels");
        assert!(html.contains("nodes:"), "embeds a nodes array");
        assert!(html.contains("links:"), "embeds a links array");
    }

    #[test]
    fn force3d_nodes_are_enriched() {
        let html = to_force3d_html(&sample_kg());
        assert!(html.contains("\"kind\""), "nodes carry a kind");
        assert!(html.contains("\"community\""), "nodes carry a community id");
    }

    #[test]
    fn force3d_federated_enriches_and_controls() {
        use crate::tests_support::kg_federated;
        let html = to_force3d_html(&kg_federated());
        assert!(html.contains("\"repo\""), "nodes carry repo");
        assert!(html.contains("\"repoColor\""), "nodes carry repoColor");
        assert!(
            html.contains("\"crossRepo\":true"),
            "a cross-repo link is flagged"
        );
        assert!(html.contains("id=\"colorby\""), "color-by toggle present");
        assert!(
            html.contains("id=\"cross-only\""),
            "cross-repo filter present"
        );
        assert!(html.contains("Repos"), "repo legend section");
    }

    #[test]
    fn force3d_single_repo_omits_repo_controls() {
        let html = to_force3d_html(&sample_kg());
        // Color-by is always present (community/kind), but the repo option and the
        // repo-only controls/legend are absent for a single-repo graph.
        assert!(html.contains("id=\"colorby\""), "color-by selector present");
        assert!(
            html.contains(">kind</option>") && !html.contains(">repo</option>"),
            "kind color option present, repo option only when federated"
        );
        assert!(
            !html.contains("id=\"cross-only\""),
            "no cross-repo filter when single-repo"
        );
        assert!(!html.contains("Repos"), "no repo legend when single-repo");
    }

    #[test]
    fn force3d_is_kind_aware() {
        let gd: synaptic_core::GraphData = serde_json::from_value(serde_json::json!({
            "nodes": [
                {"id":"app.f","label":"f()","file_type":"code","source_file":"a.py","community":0},
                {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table","community":0,"dialect":"sqlserver","rls_enabled":true},
                {"id":"sql:orders:col:total","label":"total","file_type":"code","source_file":"s.sql","kind":"column","community":0,"data_type":"int"}
            ],
            "links": [
                {"source":"app.f","target":"sql:orders","relation":"queries","confidence":"INFERRED","source_file":"a.py"},
                {"source":"sql:orders","target":"sql:orders:col:total","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}
            ]
        }))
        .unwrap();
        let html = to_force3d_html(&KnowledgeGraph::from_graph_data(gd));
        assert!(
            html.contains("data-kind=\"table\""),
            "kind filter for table"
        );
        assert!(
            html.contains("data-kind=\"column\""),
            "kind filter for column"
        );
        assert!(
            html.contains("Show SQL columns"),
            "hide-columns toggle present"
        );
        assert!(
            html.contains("bridges-only"),
            "code<->SQL bridges toggle present"
        );
        // the queries bridge edge is colored by relation and flagged bridge:true.
        assert!(
            html.contains(crate::common::relation_color("queries")),
            "bridge edge uses the relation color"
        );
        assert!(
            html.contains("\"bridge\":true"),
            "bridge flag on cross-language edge"
        );
        // SQL facts ride along for the details panel.
        assert!(
            html.contains("sqlserver") && html.contains("rls_enabled"),
            "node SQL metadata embedded"
        );
    }

    #[test]
    fn force3d_has_relation_filters() {
        // sample_kg's edges are all the "calls" relation.
        let html = to_force3d_html(&sample_kg());
        assert!(
            html.contains("data-relation=\"calls\""),
            "a filter checkbox for each present relation"
        );
    }

    #[test]
    fn force3d_has_find_panel_and_interactions() {
        let html = to_force3d_html(&sample_kg());
        // control-panel containers
        assert!(html.contains("id=\"results\""), "search results readout");
        assert!(html.contains("id=\"degree\""), "degree declutter slider");
        assert!(html.contains("id=\"details\""), "node details panel");
        // interaction wiring
        assert!(html.contains("onNodeClick"), "click-to-focus");
        assert!(html.contains("nodeVisibility"), "degree filter");
        assert!(html.contains("linkVisibility"), "relation filter");
        assert!(html.contains("cameraPosition"), "fly-to camera");
    }

    #[test]
    fn force3d_has_spread_control() {
        // A "Spread" slider expands a dense central clump: it scales the many-body
        // (charge) repulsion and the link distance, then reheats the live sim so the
        // layout re-spaces. This is layout-only (no node is hidden), unlike the
        // degree slider.
        let html = to_force3d_html(&sample_kg());
        assert!(html.contains("id=\"spread\""), "spread slider present");
        assert!(html.contains("id=\"spreadval\""), "spread readout present");
        assert!(
            html.contains("d3Force('charge')"),
            "spread scales the many-body repulsion"
        );
        assert!(
            html.contains("d3ReheatSimulation"),
            "changing spread reheats the layout so it re-spaces"
        );
    }

    #[test]
    fn force3d_lazy_loads_three_for_asset_shapes() {
        let html = to_force3d_html(&sample_kg());
        // three is pulled in via a *dynamic import* (progressive enhancement),
        // NOT a <script> tag, so it can never hijack the UMD's bundled three.
        assert!(
            html.contains(&format!("import('{THREE_ESM}')")),
            "three loaded via dynamic import()"
        );
        assert!(
            html.contains("0.179.1"),
            "three version matches 3d-force-graph's bundled three"
        );
        assert!(
            !html.contains("<script src=\"https://esm.sh"),
            "three must NOT be a <script> tag (would override window.THREE)"
        );
        assert!(
            !html.contains("email protected"),
            "the esm url must not be email-obfuscated"
        );
        assert!(
            html.contains(".catch("),
            "import failure degrades to spheres"
        );
        assert!(
            html.contains("nodeThreeObject"),
            "custom geometry hook present"
        );
        assert!(
            html.contains("assetMesh"),
            "per-kind asset mesh builder present"
        );
    }

    #[test]
    fn force3d_loads_force_graph_umd_only() {
        // Exactly one external <script src>: the 3d-force-graph UMD whose
        // bundled three actually works. (Loading a second three broke the viewer.)
        let html = to_force3d_html(&sample_kg());
        assert_eq!(
            html.matches("<script src=").count(),
            1,
            "only the 3d-force-graph UMD is a script tag"
        );
        assert!(html.contains(FORCE_GRAPH_CDN));
    }

    #[test]
    fn force3d_asset_node_carries_kind_and_toggle() {
        let html = to_force3d_html(&kg_with_asset());
        assert!(
            html.contains("\"assetKind\""),
            "asset nodes expose an assetKind field"
        );
        assert!(
            html.contains("stylesheet"),
            "the stylesheet asset_kind reaches the payload"
        );
        assert!(
            html.contains("id=\"show-assets\""),
            "Show-assets toggle present"
        );
        assert!(html.contains("id=\"legend\""), "shape legend present");
    }

    #[test]
    fn force3d_emits_adaptive_perf_scaling() {
        // The large-graph fast path must be wired in: a size-gated PERF flag,
        // edges-as-lines (width 0) under PERF, a coarser node sphere, an
        // off-screen warm-up, and a device-pixel-ratio cap. These are pure
        // browser-side render optimizations, no scan/size cap.
        let html = to_force3d_html(&sample_kg());
        assert!(html.contains("const PERF ="), "size-gated perf flag");
        assert!(html.contains("warmupTicks"), "off-screen layout warm-up");
        assert!(
            html.contains("nodeResolution"),
            "adaptive sphere resolution"
        );
        assert!(html.contains("setPixelRatio"), "device-pixel-ratio cap");
        assert!(
            html.contains("PERF ? 0 : 0.4"),
            "normal edges drop to GL lines (width 0) on large graphs"
        );
        assert!(
            html.contains("linkOpacity(PERF ? 0.15 : 0.3)"),
            "dimmer link haze on large graphs so the green edge mat doesn't wash out"
        );
    }

    #[test]
    fn force3d_bounds_the_simulation() {
        // The engine must settle and STOP (bounded ticks + faster decay), instead
        // of ticking for the default 15s. Each on-screen tick on a large graph is
        // a 50-100ms requestAnimationFrame handler, which spams the console with
        // "[Violation] 'requestAnimationFrame' handler took Nms". Once stopped,
        // rAF only renders (cheap), so the violations end.
        let html = to_force3d_html(&sample_kg());
        assert!(
            html.contains("cooldownTicks"),
            "bounds on-screen simulation ticks"
        );
        assert!(
            html.contains("cooldownTime"),
            "time backstop for the simulation"
        );
        assert!(html.contains("d3AlphaDecay"), "faster settle = fewer ticks");
    }

    #[test]
    fn force3d_emits_gpu_node_instancing() {
        // Large graphs collapse regular code nodes into a single InstancedMesh
        // (one draw call) with custom raycast picking, since the library can't
        // pick instanced nodes. These markers lock the instancing path in.
        let html = to_force3d_html(&sample_kg());
        assert!(
            html.contains("InstancedMesh"),
            "single instanced node cloud"
        );
        assert!(html.contains("buildInstances"), "instanced cloud builder");
        assert!(
            html.contains("setColorAt"),
            "per-instance node colors (highlight/dim/community)"
        );
        assert!(
            html.contains("enablePointerInteraction(false)"),
            "library pointer layer disabled when instancing is live"
        );
        assert!(
            html.contains("function pickNodeAt"),
            "custom raycast picker keeps click-to-focus working"
        );
        // Hover tooltips survive instancing via the same picker, throttled to one
        // hit-test per animation frame.
        assert!(html.contains("id=\"tip\""), "hover tooltip element present");
        assert!(
            html.contains("addEventListener('pointermove', onCanvasMove)"),
            "custom hover wired to the canvas"
        );
        assert!(
            html.contains("requestAnimationFrame(doHover)"),
            "hover hit-test throttled to one ray per frame"
        );
        // Click is detected from pointerdown/up (the camera controls suppress the
        // native 'click' via preventDefault), so focus must not depend on 'click'.
        assert!(
            html.contains("addEventListener('pointerdown'"),
            "click detected via pointerdown/up, not the suppressed 'click' event"
        );
        // A pinned huge bounding sphere keeps raycast picking exact as the layout
        // expands (three's cached early-out sphere otherwise goes stale).
        assert!(
            html.contains("boundingSphere = new THREE.Sphere"),
            "instanced cloud pins a bounding sphere so picking never early-outs"
        );
    }

    #[test]
    fn force3d_html_escapes_script_breakout() {
        // A label containing </script> must not break out of the <script> block.
        let html = to_force3d_html(&kg_with_label("x", "</script><b>pwn"));
        assert!(!html.contains("</script><b>pwn"), "raw breakout present");
        assert!(html.contains("<\\/script><b>pwn"), "escaped form expected");
    }

    #[test]
    fn force3d_recolors_custom_meshes_on_highlight() {
        // Asset/external nodes are custom meshes the library's nodeColor doesn't
        // touch, so the viewer repaints them itself (dim/highlight + color-by-repo)
        // via customMat/paintCustom, otherwise they'd stay lit on focus.
        let html = to_force3d_html(&kg_with_asset());
        assert!(
            html.contains("function customMat"),
            "custom-mesh material helper"
        );
        assert!(
            html.contains("paintCustom()"),
            "custom meshes repainted on highlight changes"
        );
    }

    #[test]
    fn force3d_search_respects_visibility_filters() {
        // Search must honour the degree/show-assets filters so it can't fly the
        // camera to a node that's currently hidden.
        let html = to_force3d_html(&sample_kg());
        assert!(
            html.contains("function nodeVisible"),
            "shared visibility predicate"
        );
        assert!(
            html.contains("nodeVisible(n) &&"),
            "search filters out hidden nodes"
        );
    }

    #[test]
    fn force3d_embeds_payload_via_json_parse() {
        // Large graphs parse faster from a JSON.parse('…') string than an inline JS
        // object literal; the embedded string stays script-breakout-safe.
        let html = to_force3d_html(&sample_kg());
        assert!(
            html.contains("JSON.parse('"),
            "data embedded via JSON.parse for faster parse"
        );
    }
}
