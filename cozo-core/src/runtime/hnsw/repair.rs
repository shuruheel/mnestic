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
    adjacency: FxHashMap<CompoundKey, Vec<CompoundKey>>,
    rows_left: usize,
    steps_left: usize,
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

fn reachable(edges: &[Vec<bool>], from: usize, to: usize, work: &mut usize) -> Result<bool> {
    let mut seen = vec![false; edges.len()];
    let mut stack = vec![from];
    seen[from] = true;
    while let Some(node) = stack.pop() {
        if node == to {
            return Ok(true);
        }
        for (next, connected) in edges[node].iter().enumerate() {
            *work = work
                .checked_sub(1)
                .ok_or_else(|| limit("reachability work"))?;
            if *connected && !seen[next] {
                seen[next] = true;
                stack.push(next);
            }
        }
    }
    Ok(false)
}

// Only new edges may be removed. Every removal preserves all existing paths by
// replacing that edge with a surviving path. Distance ordering is a heuristic;
// the reachability test, not that heuristic, establishes the safety condition.
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
    for &(from, to, _) in added {
        if degrees[from] <= cap {
            continue;
        }
        edges[from][to] = false;
        if reachable(edges, from, to, &mut work)? {
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
        let mut degrees = vec![0; n + 1];
        degrees[n] = n;
        prune_redundant_edges(&mut edges, &removable, &mut degrees, cap)?;
        if degrees[n] > cap {
            // An induced neighbourhood can hide an existing alternate path.
            // Search beyond it only when the cheap local proof is insufficient.
            // These witnesses never visit target, so removing its outgoing
            // edges cannot invalidate a cached witness adjacency.
            let mut witnesses = PruningWitnesses::default();
            for &(_, i, _) in &removable {
                if degrees[n] <= cap {
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
                )? {
                    edges[n][i] = false;
                    degrees[n] -= 1;
                }
            }
            if degrees[n] > cap {
                return Err(limit("degree cap cannot preserve bypass paths"));
            }
        }
        let mut ret = PriorityQueue::new();
        for (_, i, d) in removable {
            if edges[n][i] {
                ret.push(keys[i].clone(), Reverse(OrderedFloat(d)));
            }
        }
        Ok(ret)
    }

    fn hnsw_pruning_witness(
        &self,
        table: &RelationHandle,
        layer: i64,
        target: &CompoundKey,
        destination: &CompoundKey,
        sources: Vec<CompoundKey>,
        witnesses: &mut PruningWitnesses,
    ) -> Result<bool> {
        let mut seen: FxHashSet<_> = sources.iter().cloned().collect();
        let mut pending: std::collections::VecDeque<_> = sources.into();
        while let Some(node) = pending.pop_front() {
            if &node == destination {
                return Ok(true);
            }
            if !witnesses.adjacency.contains_key(&node) {
                if witnesses.adjacency.len() >= MAX_WITNESS_VERTICES {
                    return Err(limit("pruning witness vertices"));
                }
                let neighbours = self
                    .repair_neighbours_with_budget(table, layer, &node, &mut witnesses.rows_left)?
                    .into_iter()
                    .filter(|(key, live)| *live && key != target)
                    .map(|(key, _)| key)
                    .collect();
                witnesses.adjacency.insert(node.clone(), neighbours);
            }
            for next in &witnesses.adjacency[&node] {
                witnesses.steps_left = witnesses
                    .steps_left
                    .checked_sub(1)
                    .ok_or_else(|| limit("pruning witness reachability work"))?;
                if seen.insert(next.clone()) {
                    if seen.len() > MAX_WITNESS_VERTICES {
                        return Err(limit("pruning witness vertices"));
                    }
                    pending.push_back(next.clone());
                }
            }
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
        self.repair_neighbours_with_budget(table, layer, node, &mut usize::MAX)
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
        prune_bypasses(&mut edges, &added, &mut degrees, cap)?;
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
        let preferred = [1, 3]
            .into_iter()
            .map(|id| (node(id), Reverse(OrderedFloat(1.))))
            .collect();
        let before = table.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap();
        let kept = tx
            .hnsw_preserve_pruned_paths(&node(0), 0, table, &candidates, &preferred, 2)
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
            .hnsw_pruning_witness(table, 0, &node(0), &node(2), vec![node(1)], &mut budget)
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
            .hnsw_preserve_pruned_paths(&node(0), 0, table, &candidates, &preferred, 2)
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
            .hnsw_preserve_pruned_paths(&node(0), 0, table, &candidates, &preferred, 2)
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
            .hnsw_preserve_pruned_paths(&node(0), 0, table, &candidates, &preferred, 2)
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
