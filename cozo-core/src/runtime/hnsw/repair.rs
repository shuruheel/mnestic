// SPDX-License-Identifier: MPL-2.0
//! Bounded directed-path substitution when a graph vertex is removed.

use super::*;
use miette::Diagnostic;
use thiserror::Error;

const MAX_NEIGHBOURS: usize = 256;
const MAX_ADJACENCY_ROWS: usize = 4096;
const MAX_VECTOR_COMPONENTS: usize = 1_048_576;
const MAX_DISTANCE_COMPONENTS: usize = 16_777_216;
const MAX_REACH_STEPS: usize = 1_048_576;
const MAX_WITNESS_VERTICES: usize = 1024;
const MAX_WITNESS_ROWS: usize = 16_384;

struct PruningWitnesses {
    adjacency: FxHashMap<CompoundKey, WitnessNeighbours>,
    rows_left: usize,
    steps_left: usize,
}

struct WitnessNeighbours {
    outgoing: Vec<CompoundKey>,
    incoming: Vec<CompoundKey>,
}

struct WitnessOverlay<'a> {
    keys: &'a [CompoundKey],
    positions: &'a FxHashMap<CompoundKey, usize>,
    edges: &'a [Vec<bool>],
}

impl Default for PruningWitnesses {
    fn default() -> Self {
        Self {
            adjacency: FxHashMap::default(),
            rows_left: MAX_WITNESS_ROWS,
            steps_left: MAX_REACH_STEPS,
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("HNSW mutation repair limit: {0}; abort the mutation and rebuild the index or reduce the mutation scope")]
#[diagnostic(code(hnsw::repair_limit))]
struct RepairLimit(&'static str);

fn limit(reason: &'static str) -> miette::Report {
    RepairLimit(reason).into()
}

fn edge_key(layer: i64, from: &CompoundKey, to: &CompoundKey) -> Tuple {
    let mut key = vec![DataValue::from(layer)];
    for node in [from, to] {
        key.extend_from_slice(&node.0);
        key.push(DataValue::from(node.1 as i64));
        key.push(DataValue::from(node.2 as i64));
    }
    key
}

#[derive(Default)]
struct ReachabilityScratch {
    seen: Vec<bool>,
    stack: Vec<usize>,
}

impl ReachabilityScratch {
    fn reachable(
        &mut self,
        edges: &[Vec<bool>],
        from: usize,
        to: usize,
        work: &mut usize,
    ) -> Result<bool> {
        self.seen.resize(edges.len(), false);
        self.seen.fill(false);
        self.stack.clear();
        self.stack.push(from);
        self.seen[from] = true;
        while let Some(node) = self.stack.pop() {
            if node == to {
                return Ok(true);
            }
            for (next, connected) in edges[node].iter().enumerate() {
                *work = work
                    .checked_sub(1)
                    .ok_or_else(|| limit("reachability work"))?;
                if *connected && !self.seen[next] {
                    self.seen[next] = true;
                    self.stack.push(next);
                }
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
fn reachable(edges: &[Vec<bool>], from: usize, to: usize, work: &mut usize) -> Result<bool> {
    ReachabilityScratch::default().reachable(edges, from, to, work)
}

// Only new edges may be removed. Every removal preserves all existing paths by
// replacing that edge with a surviving path. Distance ordering is a heuristic;
// the reachability test, not that heuristic, establishes the safety condition.
#[cfg(test)]
fn prune_bypasses(
    edges: &mut [Vec<bool>],
    added: &[(usize, usize, f64)],
    degrees: &mut [usize],
    cap: usize,
) -> Result<()> {
    prune_redundant_edges(edges, added, degrees, cap)?;
    if degrees.iter().any(|&degree| degree > cap) {
        return Err(limit("degree cap cannot preserve bypass paths"));
    }
    Ok(())
}

fn prune_redundant_edges(
    edges: &mut [Vec<bool>],
    added: &[(usize, usize, f64)],
    degrees: &mut [usize],
    cap: usize,
) -> Result<()> {
    let mut work = MAX_REACH_STEPS;
    let mut scratch = ReachabilityScratch::default();
    for &(from, to, _) in added {
        if degrees[from] <= cap {
            continue;
        }
        edges[from][to] = false;
        if scratch.reachable(edges, from, to, &mut work)? {
            degrees[from] -= 1;
        } else {
            edges[from][to] = true;
        }
    }
    Ok(())
}

impl SessionTx<'_> {
    /// Preserve paths during incremental pruning too: a replacement insertion
    /// must not immediately discard the bridges just repaired by deletion.
    /// As in the bulk builder, shrinking does not extend the neighbourhood.
    pub(super) fn hnsw_preserve_pruned_paths(
        &self,
        target: &CompoundKey,
        layer: i64,
        table: &RelationHandle,
        candidates: &PriorityQueue<CompoundKey, OrderedFloat<f64>>,
        preferred: &PriorityQueue<CompoundKey, Reverse<OrderedFloat<f64>>>,
        cap: usize,
        inserted: Option<&CompoundKey>,
    ) -> Result<PriorityQueue<CompoundKey, Reverse<OrderedFloat<f64>>>> {
        if candidates.len() > MAX_NEIGHBOURS {
            return Err(limit("pruning vertices"));
        }
        let mut keys: Vec<_> = candidates.iter().map(|(k, _)| k.clone()).collect();
        keys.sort();
        let n = keys.len();
        let positions: FxHashMap<_, _> = keys
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, k)| (k, i))
            .collect();
        let mut edges = vec![vec![false; n + 1]; n + 1];
        let mut removable = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            edges[n][i] = true;
            removable.push((n, i, candidates.get_priority(key).unwrap().0));
            for (dest, live) in self.repair_neighbours(table, layer, key)? {
                if live {
                    if &dest == target {
                        edges[i][n] = true;
                    } else if let Some(&j) = positions.get(&dest) {
                        edges[i][j] = true;
                    }
                }
            }
        }
        removable.sort_by(|a, b| {
            preferred
                .get_priority(&keys[a.1])
                .is_some()
                .cmp(&preferred.get_priority(&keys[b.1]).is_some())
                .then_with(|| OrderedFloat(b.2).cmp(&OrderedFloat(a.2)))
                .then_with(|| a.1.cmp(&b.1))
        });
        let mut degree = n;
        let mut local_work = MAX_REACH_STEPS;
        let mut scratch = ReachabilityScratch::default();
        let mut witnesses = PruningWitnesses::default();
        // Earlier neighbours must not casually discard the new vertex's links:
        // that can leave a later neighbour with too many indispensable branches.
        // Try both local and external proofs for old edges before considering the
        // newly inserted edge. This is a preference, never an unremovable edge.
        // Budgets are shared across both phases, not renewed for the fallback.
        for new_edge in [false, true] {
            let phase: Vec<_> = removable
                .iter()
                .filter(|(_, i, _)| (inserted == Some(&keys[*i])) == new_edge)
                .collect();
            for &&(_, i, _) in &phase {
                if degree <= cap {
                    break;
                }
                edges[n][i] = false;
                if scratch.reachable(&edges, n, i, &mut local_work)? {
                    degree -= 1;
                } else {
                    edges[n][i] = true;
                }
            }
            // An induced neighbourhood can hide an existing alternate path.
            // Witnesses exclude target, so its outgoing-edge removals cannot
            // invalidate the stored-adjacency cache shared by these phases.
            for &&(_, i, _) in &phase {
                if degree <= cap {
                    break;
                }
                if !edges[n][i] {
                    continue;
                }
                let sources = keys
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i && edges[n][*j])
                    .map(|(_, key)| key.clone())
                    .collect();
                if self.hnsw_pruning_witness(
                    table,
                    layer,
                    target,
                    &keys[i],
                    sources,
                    &mut witnesses,
                    None,
                )? {
                    edges[n][i] = false;
                    degree -= 1;
                }
            }
        }
        if degree > cap {
            return Err(limit("insertion degree cap cannot preserve bypass paths"));
        }
        let mut ret = PriorityQueue::new();
        for (_, i, d) in removable {
            if edges[n][i] {
                ret.push(keys[i].clone(), Reverse(OrderedFloat(d)));
            }
        }
        Ok(ret)
    }

    #[allow(clippy::too_many_arguments)]
    fn hnsw_pruning_witness(
        &self,
        table: &RelationHandle,
        layer: i64,
        target: &CompoundKey,
        destination: &CompoundKey,
        sources: Vec<CompoundKey>,
        witnesses: &mut PruningWitnesses,
        overlay: Option<&WitnessOverlay<'_>>,
    ) -> Result<bool> {
        let mut forward_seen: FxHashSet<_> = sources.iter().cloned().collect();
        if forward_seen.contains(destination) {
            return Ok(true);
        }
        let mut backward_seen = FxHashSet::from_iter([destination.clone()]);
        if forward_seen.len() + backward_seen.len() > MAX_WITNESS_VERTICES {
            return Err(limit("pruning witness vertices"));
        }
        let mut forward = sources;
        let mut backward = vec![destination.clone()];
        // Expand the smaller frontier. A necessary edge into a leaf can be
        // disproved from its destination without exploring the whole component.
        // A meeting vertex proves an actual directed source -> destination path.
        while !forward.is_empty() && !backward.is_empty() {
            let reverse = backward.len() < forward.len();
            let (frontier, seen, opposite) = if reverse {
                (&mut backward, &mut backward_seen, &forward_seen)
            } else {
                (&mut forward, &mut forward_seen, &backward_seen)
            };
            let mut next_frontier = Vec::new();
            for node in frontier.drain(..) {
                if !witnesses.adjacency.contains_key(&node) {
                    if witnesses.adjacency.len() >= MAX_WITNESS_VERTICES {
                        return Err(limit("pruning witness vertices"));
                    }
                    let mut outgoing = Vec::new();
                    let mut incoming = Vec::new();
                    for (key, live) in self.repair_neighbours_with_budget(
                        table,
                        layer,
                        &node,
                        &mut witnesses.rows_left,
                    )? {
                        if &key == target {
                            continue;
                        }
                        if live {
                            outgoing.push(key.clone());
                        }
                        // Physical reverse rows identify possible predecessors,
                        // but only the opposite row's active flag proves one.
                        // Charge point reads to the same physical-row budget.
                        witnesses.rows_left = witnesses
                            .rows_left
                            .checked_sub(1)
                            .ok_or_else(|| limit("pruning witness adjacency rows"))?;
                        let value = self.repair_edge_value(table, layer, &key, &node)?
                            .ok_or_else(|| miette!("corrupt HNSW index: missing reverse edge during witness search"))?;
                        let ignored = value
                            .get(2)
                            .and_then(DataValue::get_bool)
                            .ok_or_else(|| miette!("corrupt HNSW reverse flag"))?;
                        if !ignored {
                            incoming.push(key);
                        }
                    }
                    witnesses
                        .adjacency
                        .insert(node.clone(), WitnessNeighbours { outgoing, incoming });
                }
                let stored = &witnesses.adjacency[&node];
                let stored = if reverse {
                    &stored.incoming
                } else {
                    &stored.outgoing
                };
                let neighbours: Vec<_> = if let Some((plan, from)) =
                    overlay.and_then(|plan| plan.positions.get(&node).map(|&i| (plan, i)))
                {
                    // Read the current plan in either direction; never cache
                    // proposed or previously pruned incident-to-incident edges.
                    (0..plan.keys.len())
                        .filter(|&j| {
                            if reverse {
                                plan.edges[j][from]
                            } else {
                                plan.edges[from][j]
                            }
                        })
                        .map(|j| &plan.keys[j])
                        .chain(
                            stored
                                .iter()
                                .filter(|key| !plan.positions.contains_key(*key)),
                        )
                        .collect()
                } else {
                    stored.iter().collect()
                };
                for next in neighbours {
                    witnesses.steps_left = witnesses
                        .steps_left
                        .checked_sub(1)
                        .ok_or_else(|| limit("pruning witness reachability work"))?;
                    // Check the proof before scheduling unrelated expansion.
                    if opposite.contains(next) {
                        return Ok(true);
                    }
                    if !seen.contains(next) {
                        if seen.len() + opposite.len() >= MAX_WITNESS_VERTICES {
                            return Err(limit("pruning witness vertices"));
                        }
                        seen.insert(next.clone());
                        next_frontier.push(next.clone());
                    }
                }
            }
            *frontier = next_frontier;
        }
        Ok(false)
    }

    /// During build reconciliation the base relation is already newer than the
    /// detached graph. Seed removed/changed vectors before removing any of them.
    pub(crate) fn hnsw_remove_snapshot_rows(
        &mut self,
        manifest: &HnswIndexManifest,
        orig: &RelationHandle,
        table: &RelationHandle,
        rows: &[&Tuple],
    ) -> Result<()> {
        self.hnsw_mutations_incomplete += 1;
        let mut cache = VectorCache {
            cache: FxHashMap::default(),
            distance: manifest.distance,
        };
        for row in rows {
            let key = row[..orig.metadata.keys.len()].to_vec();
            for &field in &manifest.vec_fields {
                match &row[field] {
                    DataValue::Vec(v) => {
                        cache.insert((key.clone(), field, -1), v.clone());
                    }
                    DataValue::List(values) => {
                        for (sub, value) in values.iter().enumerate() {
                            if let DataValue::Vec(v) = value {
                                cache.insert((key.clone(), field, sub as i32), v.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        for row in rows {
            self.hnsw_remove_except(
                manifest,
                orig,
                table,
                row,
                &FxHashSet::default(),
                &mut cache,
            )?;
        }
        self.hnsw_mutations_incomplete -= 1;
        Ok(())
    }

    fn repair_edge_value(
        &self,
        table: &RelationHandle,
        layer: i64,
        from: &CompoundKey,
        to: &CompoundKey,
    ) -> Result<Option<Tuple>> {
        let key = table.encode_key_for_store(&edge_key(layer, from, to), Default::default())?;
        self.idx_get(table, &key)?
            .map(|v| try_decode_val_only(&key, &v))
            .transpose()
    }

    fn repair_put_edge(
        &mut self,
        table: &RelationHandle,
        layer: i64,
        from: &CompoundKey,
        to: &CompoundKey,
        value: &[DataValue],
    ) -> Result<()> {
        let key = table.encode_key_for_store(&edge_key(layer, from, to), Default::default())?;
        let val = table.encode_val_only_for_store(value, Default::default())?;
        self.idx_put(table, &key, &val)
    }

    fn repair_neighbours(
        &self,
        table: &RelationHandle,
        layer: i64,
        node: &CompoundKey,
    ) -> Result<Vec<(CompoundKey, bool)>> {
        let mut rows_left = usize::MAX;
        self.repair_neighbours_with_budget(table, layer, node, &mut rows_left)
    }

    fn repair_neighbours_with_budget(
        &self,
        table: &RelationHandle,
        layer: i64,
        node: &CompoundKey,
        rows_left: &mut usize,
    ) -> Result<Vec<(CompoundKey, bool)>> {
        let mut prefix = vec![DataValue::from(layer)];
        prefix.extend_from_slice(&node.0);
        prefix.push(DataValue::from(node.1 as i64));
        prefix.push(DataValue::from(node.2 as i64));
        let n = node.0.len();
        let mut ret = Vec::new();
        for (count, row) in table.scan_prefix(self, &prefix).enumerate() {
            if count >= MAX_ADJACENCY_ROWS {
                return Err(limit("adjacency rows"));
            }
            *rows_left = rows_left
                .checked_sub(1)
                .ok_or_else(|| limit("pruning witness adjacency rows"))?;
            let row = row?;
            if row.len() != 2 * n + 8 {
                bail!("corrupt HNSW edge row during repair");
            }
            let other = (
                row[n + 3..2 * n + 3].to_vec(),
                row[2 * n + 3]
                    .get_int()
                    .ok_or_else(|| miette!("corrupt HNSW field"))? as usize,
                row[2 * n + 4]
                    .get_int()
                    .ok_or_else(|| miette!("corrupt HNSW subindex"))? as i32,
            );
            if &other != node {
                let ignored = row[2 * n + 7]
                    .get_bool()
                    .ok_or_else(|| miette!("corrupt HNSW edge flag"))?;
                ret.push((other, !ignored));
            }
        }
        Ok(ret)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn hnsw_repair_before_remove(
        &mut self,
        node: &CompoundKey,
        layer: i64,
        manifest: &HnswIndexManifest,
        orig: &RelationHandle,
        table: &RelationHandle,
        cache: &mut VectorCache,
    ) -> Result<()> {
        let incident = self.repair_neighbours(table, layer, node)?;
        if incident.len() > MAX_NEIGHBOURS {
            return Err(limit("incident vertices"));
        }
        let keys: Vec<_> = incident.iter().map(|(key, _)| key.clone()).collect();
        let n = keys.len();
        let positions: FxHashMap<_, _> = keys
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, k)| (k, i))
            .collect();
        let mut incoming = Vec::with_capacity(n);
        let mut edges = vec![vec![false; n]; n];
        let mut degrees = Vec::with_capacity(n);
        for (i, key) in keys.iter().enumerate() {
            let reverse = self
                .repair_edge_value(table, layer, key, node)?
                .ok_or_else(|| miette!("corrupt HNSW index: missing reverse edge during repair"))?;
            let active = !reverse[2]
                .get_bool()
                .ok_or_else(|| miette!("corrupt HNSW reverse flag"))?;
            incoming.push(active);
            let outgoing = self.repair_neighbours(table, layer, key)?;
            degrees.push(
                outgoing
                    .iter()
                    .filter(|(dest, live)| *live && dest != node)
                    .count(),
            );
            for (dest, live) in outgoing {
                if live {
                    if let Some(&j) = positions.get(&dest) {
                        edges[i][j] = true;
                    }
                }
            }
        }
        let mut added = Vec::new();
        for i in 0..n {
            for j in 0..n {
                if incoming[i] && incident[j].1 && i != j && !edges[i][j] {
                    added.push((i, j, 0.0));
                    edges[i][j] = true;
                    degrees[i] += 1;
                }
            }
        }
        if n.saturating_mul(manifest.vec_dim) > MAX_VECTOR_COMPONENTS
            || added.len().saturating_mul(manifest.vec_dim) > MAX_DISTANCE_COMPONENTS
        {
            return Err(limit("vector/distance components"));
        }
        if !added.is_empty() {
            for key in &keys {
                cache.ensure_key(key, orig, self)?;
            }
            for (from, to, distance) in &mut added {
                *distance = cache.k_dist(&keys[*from], &keys[*to]);
            }
            added.sort_by(|a, b| {
                OrderedFloat(b.2)
                    .cmp(&OrderedFloat(a.2))
                    .then_with(|| (a.0, a.1).cmp(&(b.0, b.1)))
            });
        }
        let cap = if layer == 0 {
            manifest.m_max0
        } else {
            manifest.m_max
        };
        prune_redundant_edges(&mut edges, &added, &mut degrees, cap)?;
        if degrees.iter().any(|&degree| degree > cap) {
            let mut witnesses = PruningWitnesses::default();
            for &(from, to, _) in &added {
                if degrees[from] <= cap || !edges[from][to] {
                    continue;
                }
                edges[from][to] = false;
                let plan = WitnessOverlay {
                    keys: &keys,
                    positions: &positions,
                    edges: &edges,
                };
                if self.hnsw_pruning_witness(
                    table,
                    layer,
                    node,
                    &keys[to],
                    vec![keys[from].clone()],
                    &mut witnesses,
                    Some(&plan),
                )? {
                    degrees[from] -= 1;
                } else {
                    edges[from][to] = true;
                }
            }
            if degrees.iter().any(|&degree| degree > cap) {
                return Err(limit("removal degree cap cannot preserve bypass paths"));
            }
        }
        for (from, to, distance) in added {
            if !edges[from][to] {
                continue;
            }
            self.repair_put_edge(
                table,
                layer,
                &keys[from],
                &keys[to],
                &[
                    DataValue::from(distance),
                    DataValue::Null,
                    DataValue::from(false),
                ],
            )?;
            // Retain existing reverse direction (including an ignored marker).
            if self
                .repair_edge_value(table, layer, &keys[to], &keys[from])?
                .is_none()
            {
                self.repair_put_edge(
                    table,
                    layer,
                    &keys[to],
                    &keys[from],
                    &[
                        DataValue::from(distance),
                        DataValue::Null,
                        DataValue::from(true),
                    ],
                )?;
            }
        }
        for (i, key) in keys.iter().enumerate() {
            let mut value = self
                .repair_edge_value(table, layer, key, key)?
                .ok_or_else(|| miette!("corrupt HNSW index: missing neighbour self row"))?;
            // Removal subsequently subtracts the live incoming connection to x.
            value[0] = DataValue::from((degrees[i] + usize::from(incoming[i])) as f64);
            self.repair_put_edge(table, layer, key, key, &value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reachability_scratch_resets_after_success_failure_and_exhaustion() {
        let mut edges = vec![vec![false; 3]; 3];
        edges[0][1] = true;
        edges[0][2] = true;
        let mut scratch = ReachabilityScratch::default();
        let mut work = 3;
        assert!(scratch.reachable(&edges, 0, 2, &mut work).unwrap());
        assert_eq!(work, 0);
        // Reusing buffers must not replenish the caller's shared budget.
        assert!(scratch.reachable(&edges, 0, 1, &mut work).is_err());
        edges[0][1] = false;
        assert!(!scratch.reachable(&edges, 0, 1, &mut 100).unwrap());
        edges[0][1] = true;
        assert!(scratch.reachable(&edges, 0, 1, &mut 100).unwrap());
    }

    fn script(db: &crate::DbInstance, text: &str) -> crate::NamedRows {
        db.run_script(text, Default::default(), crate::ScriptMutability::Mutable)
            .unwrap()
    }

    #[test]
    fn upper_layer_failure_cannot_commit_lower_layer_repairs() {
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        script(&db, ":create p {id:Int=>v:<F32;2>}");
        script(
            &db,
            "::hnsw create p:g {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}",
        );
        for id in 0..5 {
            script(
                &db,
                &format!("?[id,v]<-[[{id},[{id},{id}]]] :put p{{id=>v}}"),
            );
        }
        let crate::DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let mut tx = inner.transact_write().unwrap();
        let orig = tx.get_relation("p", false).unwrap();
        let (table, mut manifest) = orig.hnsw_indices.get("g").unwrap().clone();
        let rows: Vec<_> = table.scan_all(&tx).collect::<Result<_>>().unwrap();
        for row in rows {
            if row[0].get_int().unwrap() <= 0 {
                let key = table
                    .encode_key_for_store(&row[..table.metadata.keys.len()], Default::default())
                    .unwrap();
                tx.idx_del(&table, &key).unwrap();
            }
        }
        let node = |id| (vec![DataValue::from(id)], 1, -1);
        for (layer, edges) in [
            (
                0,
                vec![
                    (0, 1),
                    (1, 0),
                    (1, 2),
                    (2, 1),
                    (2, 3),
                    (3, 2),
                    (3, 4),
                    (4, 3),
                ],
            ),
            (-1, vec![(0, 1), (0, 3), (1, 2), (1, 4)]),
        ] {
            for id in 0..5 {
                let row = orig.get(&tx, &[DataValue::from(id)]).unwrap().unwrap();
                let DataValue::Vec(v) = &row[1] else {
                    unreachable!()
                };
                tx.repair_put_edge(&table,layer,&node(id),&node(id),&[
                    DataValue::from(edges.iter().filter(|(from,_)| *from==id).count() as f64),
                    DataValue::Bytes(v.get_hash().as_ref().to_vec()),DataValue::from(false)]).unwrap();
            }
            for &(from, to) in &edges {
                tx.repair_put_edge(
                    &table,
                    layer,
                    &node(from),
                    &node(to),
                    &[
                        DataValue::from(2. * ((from - to) * (from - to)) as f64),
                        DataValue::Null,
                        DataValue::from(false),
                    ],
                )
                .unwrap();
                if !edges.contains(&(to, from)) {
                    tx.repair_put_edge(
                        &table,
                        layer,
                        &node(to),
                        &node(from),
                        &[
                            DataValue::from(2. * ((from - to) * (from - to)) as f64),
                            DataValue::Null,
                            DataValue::from(true),
                        ],
                    )
                    .unwrap();
                }
            }
        }
        manifest.m_max = 2;
        tx.insert_hnsw_index_meta(orig, table, manifest, "g")
            .unwrap();
        tx.commit_tx().unwrap();
        drop(tx);
        let query = "?[l,a,f,s,b,g,t,d,h,i] := *p:g[l,a,f,s,b,g,t,d,h,i] :order l,a,f,s,b,g,t";
        let before = script(&db, query).rows;
        let tx = db.multi_transaction(true);
        let error = tx
            .run_script("?[id]<-[[1]] :rm p{id}", Default::default())
            .unwrap_err();
        assert!(format!("{error:?}").contains("hnsw::repair_limit"));
        // The lower layer really changed before the upper-layer degree failure.
        assert_ne!(
            tx.run_script(query, Default::default()).unwrap().rows,
            before
        );
        tx.run_script("?[id,v]<-[[4,[4,4]]] :put p{id=>v}", Default::default())
            .unwrap();
        assert!(tx
            .commit()
            .unwrap_err()
            .to_string()
            .contains("incomplete HNSW mutation"));
        drop(tx);
        assert_eq!(script(&db, query).rows, before);
        assert_eq!(script(&db, "?[id] := *p{id}").rows.len(), 5);
    }

    #[test]
    fn reconciliation_uses_vectors_from_deleted_snapshot_neighbours() {
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        script(&db, ":create p {id:Int=>v:<F32;2>}");
        script(
            &db,
            "::hnsw create p:g {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}",
        );
        for id in 0..4 {
            script(
                &db,
                &format!("?[id,v]<-[[{id},[{id},{id}]]] :put p{{id=>v}}"),
            );
        }
        let crate::DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let mut tx = inner.transact_write().unwrap();
        let orig = tx.get_relation("p", false).unwrap();
        let (table, manifest) = orig.hnsw_indices.get("g").unwrap();
        let snapshot: Vec<_> = orig.scan_all(&tx).collect::<Result<_>>().unwrap();
        // Simulate the live base advancing while a detached graph is built.
        for id in [1, 2] {
            let key = orig
                .encode_key_for_store(&[DataValue::from(id)], Default::default())
                .unwrap();
            tx.store_tx.del(&key).unwrap();
        }
        tx.reconcile_hnsw_index(manifest, &orig, table, None, &snapshot)
            .unwrap();
        tx.commit_tx().unwrap();
        drop(tx);
        assert_eq!(
            script(
                &db,
                "?[id] := ~p:g{id | query:vec([0,0]),k:30,ef:50} :order id"
            )
            .into_json()["rows"],
            serde_json::json!([[0], [3]])
        );
    }

    #[test]
    fn insertion_prunes_after_installing_all_selected_links() {
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        script(&db, ":create p {id:Int=>v:<F32;2>}");
        script(&db, "?[id,v]<-[[0,[0,0]],[1,[1,0]],[2,[-1,0]],[3,[0,1]],[4,[0,-1]],[5,[0.1,0.1]]] :put p{id=>v}");
        script(
            &db,
            "::hnsw create p:g {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}",
        );
        let crate::DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let mut tx = inner.transact_write().unwrap();
        let orig = tx.get_relation("p", false).unwrap();
        let (table, mut manifest) = orig.hnsw_indices.get("g").unwrap().clone();
        for row in table.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap() {
            let key = table
                .encode_key_for_store(&row[..table.metadata.keys.len()], Default::default())
                .unwrap();
            tx.idx_del(&table, &key).unwrap();
        }
        let node = |id| (vec![DataValue::from(id)], 1, -1);
        // A full star has no redundant edges until the new vertex's links to
        // multiple leaves exist. The centre is the first selected neighbour.
        for id in 0..5 {
            let row = orig.get(&tx, &[DataValue::from(id)]).unwrap().unwrap();
            let DataValue::Vec(v) = &row[1] else {
                unreachable!()
            };
            tx.repair_put_edge(
                &table,
                0,
                &node(id),
                &node(id),
                &[
                    DataValue::from(if id == 0 { 4. } else { 1. }),
                    DataValue::Bytes(v.get_hash().as_ref().to_vec()),
                    DataValue::from(false),
                ],
            )
            .unwrap();
            if id != 0 {
                for (from, to) in [(0, id), (id, 0)] {
                    tx.repair_put_edge(
                        &table,
                        0,
                        &node(from),
                        &node(to),
                        &[DataValue::from(1.), DataValue::Null, DataValue::from(false)],
                    )
                    .unwrap();
                }
            }
        }
        manifest.m_max0 = 4;
        manifest.keep_pruned_connections = true;
        let row = orig.get(&tx, &[DataValue::from(5)]).unwrap().unwrap();
        let DataValue::Vec(v) = &row[1] else {
            unreachable!()
        };
        let mut cache = VectorCache {
            cache: Default::default(),
            distance: manifest.distance,
        };
        tx.hnsw_put_vector(&row, v, 1, -1, &manifest, &orig, &table, &mut cache)
            .unwrap();
        for id in 0..6 {
            let actual = tx
                .hnsw_get_neighbours(&node(id), 0, &table, false)
                .unwrap()
                .count();
            let stored = tx
                .repair_edge_value(&table, 0, &node(id), &node(id))
                .unwrap()
                .unwrap();
            assert!(actual <= 4);
            assert_eq!(stored[0].get_float().unwrap() as usize, actual);
            let mut visited = std::collections::BTreeSet::new();
            let mut pending = vec![node(id)];
            while let Some(current) = pending.pop() {
                if visited.insert(current.clone()) {
                    pending.extend(
                        tx.hnsw_get_neighbours(&current, 0, &table, false)
                            .unwrap()
                            .map(|(key, _)| key),
                    );
                }
            }
            assert_eq!(visited.len(), 6, "reachability from {id}");
        }
    }

    #[test]
    fn removal_uses_external_witness_without_losing_survivor_paths() {
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        script(&db, ":create p {id:Int=>v:<F32;2>}");
        script(
            &db,
            "?[id,v]<-[[0,[0,0]],[1,[-1,0]],[2,[10,0]],[3,[0,1]],[4,[5,0]]] :put p{id=>v}",
        );
        script(
            &db,
            "::hnsw create p:g {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}",
        );
        let crate::DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let mut tx = inner.transact_write().unwrap();
        let orig = tx.get_relation("p", false).unwrap();
        let (table, mut manifest) = orig.hnsw_indices.get("g").unwrap().clone();
        for row in table.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap() {
            let key = table
                .encode_key_for_store(&row[..table.metadata.keys.len()], Default::default())
                .unwrap();
            tx.idx_del(&table, &key).unwrap();
        }
        let node = |id| (vec![DataValue::from(id)], 1, -1);
        let links = [(1, 0), (0, 2), (0, 3), (1, 4), (4, 2)];
        for id in 0..5 {
            let row = orig.get(&tx, &[DataValue::from(id)]).unwrap().unwrap();
            let DataValue::Vec(v) = &row[1] else {
                unreachable!()
            };
            tx.repair_put_edge(
                &table,
                0,
                &node(id),
                &node(id),
                &[
                    DataValue::from(links.iter().filter(|(from, _)| *from == id).count() as f64),
                    DataValue::Bytes(v.get_hash().as_ref().to_vec()),
                    DataValue::from(false),
                ],
            )
            .unwrap();
        }
        for (from, to) in links {
            tx.repair_put_edge(
                &table,
                0,
                &node(from),
                &node(to),
                &[DataValue::from(1.), DataValue::Null, DataValue::from(false)],
            )
            .unwrap();
            tx.repair_put_edge(
                &table,
                0,
                &node(to),
                &node(from),
                &[DataValue::from(1.), DataValue::Null, DataValue::from(true)],
            )
            .unwrap();
        }
        // Reuse stored-adjacency cache while changing the plan. Neither a
        // previously planned bypass nor a stored incident edge may reappear.
        let keys: Vec<_> = (1..5).map(node).collect();
        let positions = keys
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, key)| (key, i))
            .collect();
        let mut edges = vec![vec![false; 4]; 4];
        edges[0][3] = true; // stored 1 -> 4
        edges[3][1] = true; // stored 4 -> 2
        edges[0][2] = true; // planned 1 -> 3
                            // One source selects forward expansion; two select backward expansion.
        for sources in [vec![node(1)], vec![node(1), node(4)]] {
            edges[0][2] = true;
            edges[3][1] = true;
            let mut witnesses = PruningWitnesses::default();
            for (destination, expected) in [(3, true), (3, false), (2, true), (2, false)] {
                let plan = WitnessOverlay {
                    keys: &keys,
                    positions: &positions,
                    edges: &edges,
                };
                assert_eq!(
                    tx.hnsw_pruning_witness(
                        &table,
                        0,
                        &node(0),
                        &node(destination),
                        sources.clone(),
                        &mut witnesses,
                        Some(&plan),
                    )
                    .unwrap(),
                    expected,
                );
                if destination == 3 {
                    edges[0][2] = false;
                } else {
                    edges[3][1] = false;
                }
            }
        }
        manifest.m_max0 = 2;
        let mut cache = VectorCache {
            cache: Default::default(),
            distance: manifest.distance,
        };
        tx.hnsw_remove_vec(
            &[DataValue::from(0)],
            1,
            -1,
            &manifest,
            &orig,
            &table,
            &mut cache,
        )
        .unwrap();
        assert!(tx
            .repair_edge_value(&table, 0, &node(1), &node(2))
            .unwrap()
            .is_none());
        assert_eq!(
            tx.repair_edge_value(&table, 0, &node(1), &node(3))
                .unwrap()
                .unwrap()[2],
            DataValue::from(false)
        );
        assert_eq!(
            tx.repair_edge_value(&table, 0, &node(3), &node(1))
                .unwrap()
                .unwrap()[2],
            DataValue::from(true)
        );
        for id in 1..5 {
            let neighbours: Vec<_> = tx
                .hnsw_get_neighbours(&node(id), 0, &table, false)
                .unwrap()
                .collect();
            assert!(neighbours.len() <= 2);
            let stored = tx
                .repair_edge_value(&table, 0, &node(id), &node(id))
                .unwrap()
                .unwrap();
            assert_eq!(stored[0].get_float().unwrap() as usize, neighbours.len());
            assert!(neighbours.iter().all(|(key, _)| key != &node(0)));
        }
        let seen = tx
            .hnsw_get_neighbours(&node(1), 0, &table, false)
            .unwrap()
            .map(|(key, _)| key)
            .collect::<FxHashSet<_>>();
        assert!(seen.contains(&node(3)) && seen.contains(&node(4)));
        assert!(tx
            .hnsw_get_neighbours(&node(4), 0, &table, false)
            .unwrap()
            .any(|(key, _)| key == node(2)));
    }

    #[test]
    fn bidirectional_witness_keeps_vertex_row_and_step_limits() {
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        script(&db, ":create p {id:Int=>v:<F32;2>}");
        script(
            &db,
            "::hnsw create p:g {fields:[v],dim:2,m:16,dtype:F32,distance:L2,ef_construction:50}",
        );
        let crate::DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let mut tx = inner.transact_write().unwrap();
        let orig = tx.get_relation("p", false).unwrap();
        let (table, _) = orig.hnsw_indices.get("g").unwrap();
        let node = |id: usize| (vec![DataValue::from(id as i64)], 1, -1);
        let last = MAX_WITNESS_VERTICES + 2;
        for from in 1..last {
            for (a, b, ignored) in [(from, from + 1, false), (from + 1, from, true)] {
                tx.repair_put_edge(
                    table,
                    0,
                    &node(a),
                    &node(b),
                    &[
                        DataValue::from(1.),
                        DataValue::Null,
                        DataValue::from(ignored),
                    ],
                )
                .unwrap();
            }
        }
        for (mut budget, reason) in [
            (PruningWitnesses::default(), "witness vertices"),
            // One physical prefix row fits, but its reverse point read does not.
            (
                PruningWitnesses {
                    rows_left: 1,
                    ..Default::default()
                },
                "witness adjacency rows",
            ),
            (
                PruningWitnesses {
                    steps_left: 0,
                    ..Default::default()
                },
                "witness reachability work",
            ),
        ] {
            let error = tx
                .hnsw_pruning_witness(
                    table,
                    0,
                    &node(0),
                    &node(last),
                    vec![node(1)],
                    &mut budget,
                    None,
                )
                .unwrap_err();
            assert!(error.to_string().contains(reason), "{error:?}");
            assert!(budget.adjacency.len() <= MAX_WITNESS_VERTICES);
        }
    }

    #[test]
    fn insertion_pruning_preserves_new_links_until_old_proofs_are_exhausted() {
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        script(&db, ":create p {id:Int=>v:<F32;2>}");
        script(
            &db,
            "::hnsw create p:g {fields:[v],dim:2,m:2,dtype:F32,distance:L2,ef_construction:50}",
        );
        let crate::DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let mut tx = inner.transact_write().unwrap();
        let orig = tx.get_relation("p", false).unwrap();
        let (table, _) = orig.hnsw_indices.get("g").unwrap();
        // N=0, A=1, B=2, C=3, D=4, E=5. Existing graph is strongly
        // connected and capped at two. N selects A and B reciprocally.
        let node = |id| (vec![DataValue::from(id)], 1, -1);
        let links = [
            (1, 2),
            (1, 5),
            (2, 3),
            (2, 4),
            (3, 1),
            (4, 2),
            (5, 1),
            (0, 1),
            (0, 2),
            (1, 0),
            (2, 0),
        ];
        for &(a, b) in &links {
            for (from, to) in [(a, b), (b, a)] {
                tx.repair_put_edge(
                    table,
                    0,
                    &node(from),
                    &node(to),
                    &[DataValue::from(1.), DataValue::Null, DataValue::from(true)],
                )
                .unwrap();
            }
        }
        for &(a, b) in &links {
            tx.repair_put_edge(
                table,
                0,
                &node(a),
                &node(b),
                &[DataValue::from(1.), DataValue::Null, DataValue::from(false)],
            )
            .unwrap();
        }
        let a = [0, 2, 5]
            .into_iter()
            .map(|id| (node(id), OrderedFloat(1.)))
            .collect();
        let b = [0, 3, 4]
            .into_iter()
            .map(|id| (node(id), OrderedFloat(1.)))
            .collect();
        let greedy = tx
            .hnsw_preserve_pruned_paths(&node(1), 0, table, &a, &PriorityQueue::new(), 2, None)
            .unwrap();
        assert!(greedy.get_priority(&node(0)).is_none());
        // A->N was validly removed via A->B->N. That decision leaves B
        // with three indispensable branches, although a joint solution exists.
        tx.repair_put_edge(
            table,
            0,
            &node(1),
            &node(0),
            &[DataValue::from(1.), DataValue::Null, DataValue::from(true)],
        )
        .unwrap();
        let error = tx
            .hnsw_preserve_pruned_paths(&node(2), 0, table, &b, &PriorityQueue::new(), 2, None)
            .unwrap_err();
        assert!(format!("{error}").contains("insertion degree cap cannot preserve bypass paths"));
        // Undo A's choice. Keeping its new link instead allows A->B to
        // disappear through A->N->B, then B->N through B->C->A->N.
        tx.repair_put_edge(
            table,
            0,
            &node(1),
            &node(0),
            &[DataValue::from(1.), DataValue::Null, DataValue::from(false)],
        )
        .unwrap();
        let alternative = tx
            .hnsw_preserve_pruned_paths(
                &node(1),
                0,
                table,
                &a,
                &PriorityQueue::new(),
                2,
                Some(&node(0)),
            )
            .unwrap();
        assert!(alternative.get_priority(&node(2)).is_none());
        tx.repair_put_edge(
            table,
            0,
            &node(1),
            &node(2),
            &[DataValue::from(1.), DataValue::Null, DataValue::from(true)],
        )
        .unwrap();
        let last = tx
            .hnsw_preserve_pruned_paths(
                &node(2),
                0,
                table,
                &b,
                &PriorityQueue::new(),
                2,
                Some(&node(0)),
            )
            .unwrap();
        assert_eq!(last.len(), 2);
        assert!(last.get_priority(&node(0)).is_none());
        tx.repair_put_edge(
            table,
            0,
            &node(2),
            &node(0),
            &[DataValue::from(1.), DataValue::Null, DataValue::from(true)],
        )
        .unwrap();
        let mut edges = vec![vec![false; 6]; 6];
        for (i, row) in edges.iter_mut().enumerate() {
            for (dest, live) in tx.repair_neighbours(table, 0, &node(i as i64)).unwrap() {
                if live {
                    row[dest.0[0].get_int().unwrap() as usize] = true;
                }
            }
            assert!(row.iter().filter(|&&live| live).count() <= 2);
        }
        let mut work = MAX_REACH_STEPS;
        for i in 0..6 {
            for j in 0..6 {
                assert!(reachable(&edges, i, j, &mut work).unwrap());
            }
        }
    }

    #[test]
    fn insertion_tries_external_old_edge_proofs_before_new_edge_local_proof() {
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        script(&db, ":create p {id:Int=>v:<F32;2>}");
        script(
            &db,
            "::hnsw create p:g {fields:[v],dim:2,m:2,dtype:F32,distance:L2,ef_construction:50}",
        );
        let crate::DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let mut tx = inner.transact_write().unwrap();
        let orig = tx.get_relation("p", false).unwrap();
        let (table, _) = orig.hnsw_indices.get("g").unwrap();
        let node = |id| (vec![DataValue::from(id)], 1, -1);
        let links = [
            (1, 0),
            (1, 2),
            (1, 3),
            (2, 0),
            (0, 1),
            (0, 4),
            (4, 2),
            (3, 1),
        ];
        for &(a, b) in &links {
            for (from, to) in [(a, b), (b, a)] {
                tx.repair_put_edge(
                    table,
                    0,
                    &node(from),
                    &node(to),
                    &[DataValue::from(1.), DataValue::Null, DataValue::from(true)],
                )
                .unwrap();
            }
        }
        for &(a, b) in &links {
            tx.repair_put_edge(
                table,
                0,
                &node(a),
                &node(b),
                &[DataValue::from(1.), DataValue::Null, DataValue::from(false)],
            )
            .unwrap();
        }
        let before = table.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap();
        let candidates = [0, 2, 3]
            .into_iter()
            .map(|id| (node(id), OrderedFloat(1.)))
            .collect();
        // 1->0 has a local alternate 1->2->0. Keep it until the external
        // alternate 1->0->4->2 for old edge 1->2 has been considered.
        let kept = tx
            .hnsw_preserve_pruned_paths(
                &node(1),
                0,
                table,
                &candidates,
                &PriorityQueue::new(),
                2,
                Some(&node(0)),
            )
            .unwrap();
        assert_eq!(kept.len(), 2);
        assert!(kept.get_priority(&node(0)).is_some());
        assert!(kept.get_priority(&node(3)).is_some());
        assert_eq!(
            before,
            table.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap()
        );
    }

    #[test]
    fn pruning_witness_does_not_explore_a_wide_component_before_a_short_proof() {
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        script(&db, ":create p {id:Int=>v:<F32;2>}");
        script(
            &db,
            "::hnsw create p:g {fields:[v],dim:2,m:16,dtype:F32,distance:L2,ef_construction:50}",
        );
        let crate::DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let mut tx = inner.transact_write().unwrap();
        let orig = tx.get_relation("p", false).unwrap();
        let (table, _) = orig.hnsw_indices.get("g").unwrap();
        let node = |id| (vec![DataValue::from(id)], 1, -1);
        let mut links = vec![(0, 1), (0, 3), (0, 9999), (1, 4), (4, 3)];
        // Every stored vertex except the deliberately over-cap pruning target
        // stays within 32 outgoing edges. The unrelated component is wide
        // enough to exhaust a forward-only 1024-vertex search.
        for branch in 100..131 {
            links.push((1, branch));
            for leaf in 0..32 {
                links.push((branch, 1000 + (branch - 100) * 32 + leaf));
            }
        }
        for &(from, to) in &links {
            for (a, b, ignored) in [(from, to, false), (to, from, true)] {
                tx.repair_put_edge(
                    table,
                    0,
                    &node(a),
                    &node(b),
                    &[
                        DataValue::from(1.),
                        DataValue::Null,
                        DataValue::from(ignored),
                    ],
                )
                .unwrap();
            }
        }
        let before = table.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap();
        let mut witnesses = PruningWitnesses::default();
        assert!(tx
            .hnsw_pruning_witness(
                table,
                0,
                &node(0),
                &node(3),
                vec![node(1)],
                &mut witnesses,
                None,
            )
            .unwrap());
        // 9999 is reachable only through excluded target 0. Retain that edge,
        // then prove the redundant 0 -> 3 through 0 -> 1 -> 4 -> 3.
        let candidates = [(1, 1.), (3, 90.), (9999, 100.)]
            .into_iter()
            .map(|(id, d)| (node(id), OrderedFloat(d)))
            .collect();
        let kept = tx
            .hnsw_preserve_pruned_paths(
                &node(0),
                0,
                table,
                &candidates,
                &PriorityQueue::new(),
                2,
                None,
            )
            .unwrap();
        assert_eq!(kept.len(), 2);
        assert!(kept.get_priority(&node(1)).is_some());
        assert!(kept.get_priority(&node(9999)).is_some());
        assert_eq!(
            before,
            table.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap()
        );
    }

    #[test]
    fn incremental_pruning_finds_external_witness_without_changing_edges() {
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        script(&db, ":create p {id:Int=>v:<F32;2>}");
        script(
            &db,
            "?[id,v]<-[[0,[0,0]],[1,[-1,0]],[2,[10,0]],[3,[0,1]],[4,[5,0]]] :put p{id=>v}",
        );
        script(
            &db,
            "::hnsw create p:g {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}",
        );
        let crate::DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let mut tx = inner.transact_write().unwrap();
        let orig = tx.get_relation("p", false).unwrap();
        let (table, _) = orig.hnsw_indices.get("g").unwrap();
        for row in table.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap() {
            let key = table
                .encode_key_for_store(&row[..table.metadata.keys.len()], Default::default())
                .unwrap();
            tx.idx_del(table, &key).unwrap();
        }
        let node = |id| (vec![DataValue::from(id)], 1, -1);
        // No path between 1,2,3 is visible in their induced subgraph. But
        // 0->1->4->2 makes 0->2 redundant in the actual graph.
        for (from, to) in [
            (0, 1),
            (1, 0),
            (0, 2),
            (2, 0),
            (0, 3),
            (3, 0),
            (1, 4),
            (4, 2),
        ] {
            tx.repair_put_edge(
                table,
                0,
                &node(from),
                &node(to),
                &[DataValue::from(1.), DataValue::Null, DataValue::from(false)],
            )
            .unwrap();
        }
        let candidates = (1..4).map(|id| (node(id), OrderedFloat(1.))).collect();
        // The production index stores bookkeeping rows for both directions,
        // even when only one direction is traversable.
        for (from, to) in [(4, 1), (2, 4)] {
            tx.repair_put_edge(
                table,
                0,
                &node(from),
                &node(to),
                &[DataValue::from(1.), DataValue::Null, DataValue::from(true)],
            )
            .unwrap();
        }
        let preferred = [1, 3]
            .into_iter()
            .map(|id| (node(id), Reverse(OrderedFloat(1.))))
            .collect();
        let before = table.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap();
        let kept = tx
            .hnsw_preserve_pruned_paths(&node(0), 0, table, &candidates, &preferred, 2, None)
            .unwrap();
        assert_eq!(kept.len(), 2);
        assert!(kept.get_priority(&node(1)).is_some());
        assert!(kept.get_priority(&node(3)).is_some());
        assert_eq!(
            before,
            table.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap()
        );
        let mut budget = PruningWitnesses {
            rows_left: 0,
            ..Default::default()
        };
        let error = tx
            .hnsw_pruning_witness(
                table,
                0,
                &node(0),
                &node(2),
                vec![node(1)],
                &mut budget,
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("witness adjacency rows"));
        // An ignored edge is bookkeeping, never a valid witness.
        tx.repair_put_edge(
            table,
            0,
            &node(4),
            &node(2),
            &[DataValue::from(1.), DataValue::Null, DataValue::from(true)],
        )
        .unwrap();
        let error = tx
            .hnsw_preserve_pruned_paths(&node(0), 0, table, &candidates, &preferred, 2, None)
            .unwrap_err();
        assert!(error.to_string().contains("degree cap"));
    }

    #[test]
    fn incremental_pruning_refuses_to_strand_a_leaf() {
        let db = crate::DbInstance::new("mem", "", "").unwrap();
        script(&db, ":create p {id:Int=>v:<F32;2>}");
        script(
            &db,
            "::hnsw create p:g {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}",
        );
        script(
            &db,
            "?[id,v]<-[[0,[0,0]],[1,[-1,0]],[2,[10,0]],[3,[0,1]]] :put p{id=>v}",
        );
        let crate::DbInstance::Mem(inner) = &db else {
            unreachable!()
        };
        let mut tx = inner.transact_write().unwrap();
        let orig = tx.get_relation("p", false).unwrap();
        let (table, _) = orig.hnsw_indices.get("g").unwrap();
        let rows: Vec<_> = table.scan_all(&tx).collect::<Result<_>>().unwrap();
        for row in rows {
            let key = table
                .encode_key_for_store(&row[..table.metadata.keys.len()], Default::default())
                .unwrap();
            tx.idx_del(table, &key).unwrap();
        }
        let node = |id| (vec![DataValue::from(id)], 1, -1);
        let mut candidates = PriorityQueue::new();
        for id in 1..4 {
            tx.repair_put_edge(
                table,
                0,
                &node(0),
                &node(id),
                &[DataValue::from(1.), DataValue::Null, DataValue::from(false)],
            )
            .unwrap();
            tx.repair_put_edge(
                table,
                0,
                &node(id),
                &node(0),
                &[DataValue::from(1.), DataValue::Null, DataValue::from(false)],
            )
            .unwrap();
            candidates.push(node(id), OrderedFloat(1.));
        }
        let preferred = [
            (node(1), Reverse(OrderedFloat(1.))),
            (node(3), Reverse(OrderedFloat(1.))),
        ]
        .into_iter()
        .collect();
        let error = tx
            .hnsw_preserve_pruned_paths(&node(0), 0, table, &candidates, &preferred, 2, None)
            .unwrap_err();
        assert!(error.to_string().contains("degree cap"));
        // A surviving path through 1 now permits removal of the direct 0->2.
        tx.repair_put_edge(
            table,
            0,
            &node(1),
            &node(2),
            &[
                DataValue::from(121.),
                DataValue::Null,
                DataValue::from(false),
            ],
        )
        .unwrap();
        let kept = tx
            .hnsw_preserve_pruned_paths(&node(0), 0, table, &candidates, &preferred, 2, None)
            .unwrap();
        assert!(kept.get_priority(&node(1)).is_some() && kept.get_priority(&node(3)).is_some());
    }

    #[test]
    fn pruning_preserves_a_bypass_via_another_neighbour() {
        let mut edges = vec![
            vec![false, true, true],
            vec![false, false, true],
            vec![false, false, false],
        ];
        let mut degree = vec![2, 1, 0];
        prune_bypasses(&mut edges, &[(0, 2, 4.0), (0, 1, 1.0)], &mut degree, 1).unwrap();
        let mut work = MAX_REACH_STEPS;
        assert!(reachable(&edges, 0, 2, &mut work).unwrap());
        assert_eq!(degree, vec![1, 1, 0]);
    }

    #[test]
    fn necessary_bridges_are_never_pruned_to_meet_a_cap() {
        let mut edges = vec![
            vec![false, true, true],
            vec![false, false, false],
            vec![false, false, false],
        ];
        assert!(
            prune_bypasses(&mut edges, &[(0, 2, 4.0), (0, 1, 1.0)], &mut [2, 0, 0], 1).is_err()
        );
        assert!(edges[0][1] && edges[0][2]);
    }

    #[test]
    fn reachability_budget_is_enforced() {
        assert!(reachable(&[vec![false, true], vec![false, false]], 0, 1, &mut 0).is_err());
    }
}
