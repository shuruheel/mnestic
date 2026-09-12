/*
 * Flatten: materialize the net effect of a windowed view into a stack's top layer.
 *
 * One primitive covers merge-down, changeset extraction and cherry-pick; they differ only in
 * the windows of the source view.
 */

use miette::{bail, miette, IntoDiagnostic, Result, WrapErr};

use std::borrow::Cow;
use std::ops::ControlFlow;

use crate::data::memcmp::{restamp_tail_validity, tail_validity, validity_identity};
use crate::data::tuple::decode_tuple_from_key;
use crate::data::value::DataValue;
use crate::runtime::relation::try_extend_tuple_from_v;
use crate::storage::layered::iter::{LayeredTxn, StackMerge};
use crate::storage::layered::catalog::{catalog_relations, Catalog, RelInfo};
use crate::storage::layered::tx::{bind_layers, BoundLayer, BoundStack};
use crate::storage::layered::{LayeredStorage, Seq, Stack, DEFAULT_LAYER};
use crate::Db;

/// What a flatten did. A consumer recording a merge needs to know which sequences the copied
/// rows occupy, hence `seq_range`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FlattenStats {
    /// Rows written into the destination's top layer.
    pub rows_copied: u64,
    /// Bytes of key and value written.
    pub bytes_copied: u64,
    /// Identical re-introductions of a record the destination already holds live.
    pub rows_deduped: u64,
    /// Retractions that bit nothing in the destination and were therefore dropped.
    pub tombstones_dropped: u64,
    /// The sequences the copied rows occupy. `None` exactly when `restamp` was false.
    pub seq_range: Option<(Seq, Seq)>,
}

/// One value claimed for a key, and the layer claiming it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Claim {
    /// The value columns.
    pub value: Vec<DataValue>,
    /// The layer holding this value.
    pub layer: String,
}

/// One key that more than one layer claims a different value for.
///
/// Under value immutability a key's value never changes, so this never arises within one
/// lineage. It arises between lineages: each window predates the other's write, so every claim
/// was legal where it was made, and none of them is privileged. Choosing between them needs a
/// policy the storage layer does not have, so it reports every claim and refuses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlattenConflict {
    /// The relation the key belongs to.
    pub relation: String,
    /// The key, decoded. Its last element is the validity the view holds the row at.
    pub key: Vec<DataValue>,
    /// Every distinct value claimed for this key, with the layer claiming it. Always two or
    /// more, and always complete: both the source view and the destination are consulted, so
    /// resolving one claim cannot reveal another that was hidden.
    pub claims: Vec<Claim>,
}

/// What a flatten *would* do, computed without writing anything.
///
/// This is the same computation [`Db::flatten`] performs before it writes: resolving the
/// view's net effect and deciding each row against the destination's liveness. A caller
/// that needs to preview a merge, or to report its conflicts, does not have to reproduce it.
///
/// A plan is not a lock. It is subject to the same caveat as the flatten itself: nothing here
/// is atomic against concurrent writers on either stack, so a plan can be made stale by a write
/// that lands between planning and acting.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FlattenPlan {
    /// What the flatten would report, had it run. `seq_range` is always `None`: a plan occupies
    /// no sequence because it commits nothing.
    pub stats: FlattenStats,
    /// Every collision, not merely the first. Empty exactly when the flatten would succeed.
    pub conflicts: Vec<FlattenConflict>,
}

impl FlattenPlan {
    /// Whether the flatten this plans would succeed.
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}


/// A row of a paginated preview, decoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FlattenItem {
    /// A row the flatten would write into the destination's top layer.
    Copy {
        /// The relation the row belongs to.
        relation: String,
        /// The key, decoded. Its last element is the validity the view holds the row at.
        key: Vec<DataValue>,
        /// The value columns. Empty for a retraction.
        value: Vec<DataValue>,
        /// Whether the row asserts. A diff reads this as added rather than removed, without
        /// having to decode the validity back out of the key.
        asserts: bool,
    },
    /// A record the destination already holds live, identically.
    Dedupe {
        /// The relation the row belongs to.
        relation: String,
        /// The key, decoded.
        key: Vec<DataValue>,
    },
    /// A retraction that bites nothing in the destination.
    TombstoneDropped {
        /// The relation the row belongs to.
        relation: String,
        /// The key, decoded.
        key: Vec<DataValue>,
    },
    /// A key the two lineages gave different values.
    Conflict(FlattenConflict),
}

/// Where a paginated scan left off. Opaque, and meaningful only to the same `(src, dst)` pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlattenCursor(Vec<u8>);

impl FlattenCursor {
    /// The token's bytes, for a caller that needs to carry it across a process boundary.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    /// Rebuild a cursor from [`FlattenCursor::as_bytes`].
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        FlattenCursor(bytes)
    }
}

/// One page of a preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlattenPage {
    /// The items in this page, in key order.
    pub items: Vec<FlattenItem>,
    /// Where to resume, or `None` at the end of the scan.
    pub next: Option<FlattenCursor>,
}

/// What one resolved source row means against the destination, borrowed from the scan.
///
/// Nothing here is owned: a caller that counts rows allocates nothing, and a caller that
/// renders them chooses what to build. Conflicts are bounded by construction; rows are not.
enum RowEffect<'r> {
    /// The destination does not already agree: applying this row would change it.
    Effective {
        key: &'r [u8],
        val: &'r [u8],
        /// Whether the row asserts. A diff reads this as added versus removed.
        asserts: bool,
    },
    /// The destination already holds this record, identically and live.
    Redundant { key: &'r [u8] },
    /// A retraction of a record the destination never held live.
    Inert { key: &'r [u8] },
    /// One key with more than one claimed value. Conflicts are bounded by construction, so
    /// unlike the other arms this one is built rather than borrowed.
    Divergent(FlattenConflict),
}

/// The O(1) state one identity accumulates while its versions stream past.
struct Resolved {
    identity: Vec<u8>,
    /// The newest visible version: what the view resolves this identity to.
    key: Vec<u8>,
    val: Vec<u8>,
    asserts: bool,
    /// The newest assertive value and the layer holding it. Under value immutability every
    /// assertion under this key should carry the same value.
    asserted: Option<(Vec<u8>, Option<usize>)>,
    /// Any assertion carrying a different value, with its layer. Empty unless the source view
    /// disagrees with itself, so the ordinary path allocates nothing for it.
    others: Vec<(Vec<u8>, Option<usize>)>,
}

/// The name of a layer by index, for labelling a claim.
fn layer_name(layers: &[BoundLayer<'_>], at: Option<usize>) -> String {
    at.and_then(|i| layers.get(i))
        .map(|l| l.name.clone())
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// Decide one resolved identity against the destination and hand it to the visitor.
fn emit<F>(
    txn: &LayeredTxn<'_>,
    src_layers: &[BoundLayer<'_>],
    dst: &mut BoundStack<'_>,
    rel: &RelInfo,
    done: Resolved,
    visit: &mut F,
) -> Result<ControlFlow<Vec<u8>>>
where
    F: FnMut(&RelInfo, RowEffect<'_>) -> Result<ControlFlow<()>>,
{
    // The destination is consulted whatever the source view says, so a key that both sides
    // disagree about reports every claim at once rather than one now and one on a retry.
    let state = dst.key_state(txn, &done.key)?;
    let dst_disagrees = done.asserts && matches!(&state.asserted, Some(v) if *v != done.val);

    let effect = if !done.others.is_empty() || dst_disagrees {
        let mut claims = Vec::with_capacity(done.others.len() + 2);
        if let Some((val, at)) = &done.asserted {
            claims.push(Claim {
                value: decode_values(val)?,
                layer: layer_name(src_layers, *at),
            });
        }
        for (val, at) in &done.others {
            claims.push(Claim {
                value: decode_values(val)?,
                layer: layer_name(src_layers, *at),
            });
        }
        if let Some(val) = &state.asserted {
            let already = done
                .asserted
                .iter()
                .map(|(v, _)| v)
                .chain(done.others.iter().map(|(v, _)| v))
                .any(|v| v == val);
            if !already {
                claims.push(Claim {
                    value: decode_values(val)?,
                    layer: layer_name(&dst.layers, state.asserted_layer),
                });
            }
        }
        RowEffect::Divergent(FlattenConflict {
            relation: rel.name.to_string(),
            key: decode_tuple_from_key(&done.key, rel.n_keys),
            claims,
        })
    } else if done.asserts {
        if state.asserted.is_some() && state.live {
            // Already there, identical: the same record cherry-picked into a lineage that
            // already carries it.
            RowEffect::Redundant { key: &done.key }
        } else {
            RowEffect::Effective {
                key: &done.key,
                val: &done.val,
                asserts: true,
            }
        }
    } else if state.live {
        RowEffect::Effective {
            key: &done.key,
            val: &done.val,
            asserts: false,
        }
    } else {
        // A tombstone for a record this lineage never saw. Dropping it is what keeps every
        // flatten simpler than the union of its inputs.
        RowEffect::Inert { key: &done.key }
    };

    Ok(match visit(rel, effect)? {
        ControlFlow::Break(()) => ControlFlow::Break(done.key),
        ControlFlow::Continue(()) => ControlFlow::Continue(()),
    })
}

/// Resolve the source view and decide each row against the destination, one row at a time.
///
/// This is the whole of a flatten except the writing. `visit` sees every row in key order and
/// may stop early; the returned key is where a later call should resume from, and is `None`
/// when the scan ran to the end.
///
/// `relations` is the catalog, every relation the store holds, read once per transaction.
/// It is the scan's outer loop. Each entry gives the key range for that relation's rows,
/// the policy for it (`is_index`, `stackable`), and the `RelInfo` passed on to `visit`.
///
/// It must be ordered by relation id. Ids sit big-endian at the head of every key, so id
/// order is keyspace order, which is what lets `after` be a single key: a relation an earlier
/// page finished is skipped by comparing `after` against its upper bound.
///
/// Resolution happens *within the view first*: a record created and retracted inside the
/// window contributes its tombstone and only that, which is why the merge is drained per
/// identity rather than per row.
fn scan<F>(
    txn: &LayeredTxn<'_>,
    src_layers: &[BoundLayer<'_>],
    dst: &mut BoundStack<'_>,
    relations: &Catalog,
    after: Option<&[u8]>,
    mut visit: F,
) -> Result<Option<Vec<u8>>>
where
    F: FnMut(&RelInfo, RowEffect<'_>) -> Result<ControlFlow<()>>,
{
    for rel in relations.iter() {
        if rel.is_index {
            // Copying index rows would splice two independently evolved graphs into something
            // structurally invalid, whose only symptom is silent recall loss. The destination's
            // indexes are dropped and rebuilt instead.
            continue;
        }
        // `raw_encode` yields an array, so these bounds live on the stack.
        let lower = rel.id.raw_encode();
        let upper = rel.id.next().raw_encode();
        if let Some(after) = after {
            if after >= upper.as_slice() {
                // A relation an earlier page already finished.
                continue;
            }
        }
        if !rel.stackable {
            // A relation with no validity has no retraction mechanism and no stamp, so there is
            // no net effect to compute. Leaving its rows behind silently would be the worst
            // outcome, so check that there are none rather than assume it.
            if holds_rows(txn, &src_layers[0], rel)? {
                bail!(
                    "cannot flatten: relation '{}' has no validity column, and the source \
                     layer holds rows for it",
                    rel.name
                );
            }
            continue;
        }

        // Resume past every version of the last key reported, not merely past that key: older
        // versions of one identity sort *after* the newest, so seeking to the key itself would
        // re-resolve the identity to a stale version.
        let iters = src_layers
            .iter()
            .map(|l| (txn.raw_iterator_cf(&l.cf), l.window))
            .collect();
        let mut merge = StackMerge::new(iters, Some(Cow::Borrowed(&upper[..])));
        match after {
            Some(after) if after > lower.as_slice() => merge.seek(dst.range_end(after))?,
            _ => merge.seek(&lower[..])?,
        }

        // One identity at a time, and only O(1) of it: the newest version, the newest
        // assertive value, and one assertion that disagrees with it. Never the chain: a
        // record asserted and retracted many times has a long one.
        let mut cur: Option<Resolved> = None;
        while let Some(row) = merge.next_borrowed() {
            let (at_layer, key, val) = row?;
            let Some(vld) = tail_validity(key) else {
                bail!(
                    "relation '{}' has no validity but holds rows in a view being flattened; \
                     a relation without one cannot express a cross-layer retraction",
                    rel.name
                );
            };
            let asserts = vld.is_assert.0;
            let identity = validity_identity(key);

            match cur.as_mut() {
                // An older version of the identity being resolved. It does not change what the
                // view resolves to, but if it asserts a *different* value than the newest
                // assertion then two layers of the source disagree, and stamp order is not
                // entitled to pick between them.
                Some(c) if c.identity == identity => {
                    if asserts {
                        match &c.asserted {
                            None => c.asserted = Some((val.to_vec(), Some(at_layer))),
                            Some((newest, _)) if newest != val => {
                                if !c.others.iter().any(|(seen, _)| seen == val) {
                                    c.others.push((val.to_vec(), Some(at_layer)));
                                }
                            }
                            Some(_) => {}
                        }
                    }
                }
                _ => {
                    if let Some(done) = cur.take() {
                        if let ControlFlow::Break(at) =
                            emit(txn, src_layers, dst, rel, done, &mut visit)?
                        {
                            return Ok(Some(at));
                        }
                    }
                    cur = Some(Resolved {
                        identity: identity.to_vec(),
                        asserted: if asserts {
                            Some((val.to_vec(), Some(at_layer)))
                        } else {
                            None
                        },
                        key: key.to_vec(),
                        val: val.to_vec(),
                        asserts,
                        others: vec![],
                    });
                }
            }
        }
        if let Some(done) = cur.take() {
            if let ControlFlow::Break(at) = emit(txn, src_layers, dst, rel, done, &mut visit)? {
                return Ok(Some(at));
            }
        }
    }
    Ok(None)
}

/// A stored value blob, decoded. Fallible for the same reason the read path is: a row that
/// will not decode must not take the whole scan down with it.
fn decode_values(val: &[u8]) -> Result<Vec<DataValue>> {
    let mut ret = vec![];
    try_extend_tuple_from_v(&mut ret, val)?;
    Ok(ret)
}

/// Everything a scan needs, bound for the life of one call.
///
/// Every field borrows from the open database rather than from a sibling, so this is an
/// ordinary struct. It is returned rather than lent to a callback because
/// `Transaction::commit` consumes the transaction.
struct ScanContext<'a> {
    txn: LayeredTxn<'a>,
    src_layers: Vec<BoundLayer<'a>>,
    dst: BoundStack<'a>,
    relations: Catalog,
}

impl Db<LayeredStorage> {
    fn scan_context<'a>(&'a self, src: &Stack, dst: &Stack) -> Result<ScanContext<'a>> {
        let src_spec = self.db.resolve(src)?;
        let dst_spec = self.db.resolve(dst)?;
        let inner = &*self.db.inner;
        let txn = inner.db.transaction();
        let src_layers = bind_layers(inner, &src_spec)?;
        let dst = BoundStack::new(bind_layers(inner, &dst_spec)?, dst_spec.view.clone());
        let catalog = inner
            .db
            .cf_handle(DEFAULT_LAYER)
            .ok_or_else(|| miette!("the default layer is missing"))?;
        let relations = catalog_relations(&txn, &catalog)?;
        Ok(ScanContext {
            txn,
            src_layers,
            dst,
            relations,
        })
    }

    /// What [`Db::flatten`] would do, without doing it.
    ///
    /// Every check a flatten makes runs here: the view's net effect, dedupe, and tombstone
    /// liveness against the whole destination stack. A merge can be previewed, and its
    /// conflicts reported in full, without the caller reimplementing any of it.
    ///
    /// Memory is proportional to the number of conflicts, not to the size of the view: rows are
    /// counted as they stream past. Use [`Db::flatten_page`] to see the rows themselves.
    ///
    /// A clean plan is not a promise: see [`FlattenPlan`] on staleness.
    pub fn flatten_plan(&self, src: &Stack, dst: &Stack) -> Result<FlattenPlan> {
        let mut ctx = self.scan_context(src, dst)?;
        let mut plan = FlattenPlan::default();
        scan(
            &ctx.txn,
            &ctx.src_layers,
            &mut ctx.dst,
            &ctx.relations,
            None,
            |_rel, effect| {
                match effect {
                    RowEffect::Effective { key, val, .. } => {
                        plan.stats.rows_copied += 1;
                        plan.stats.bytes_copied += (key.len() + val.len()) as u64;
                    }
                    RowEffect::Redundant { .. } => plan.stats.rows_deduped += 1,
                    RowEffect::Inert { .. } => plan.stats.tombstones_dropped += 1,
                    RowEffect::Divergent(conflict) => plan.conflicts.push(conflict),
                }
                Ok(ControlFlow::Continue(()))
            },
        )?;
        Ok(plan)
    }

    /// One page of what [`Db::flatten`] would do, decoded.
    ///
    /// Pass `after: None` for the first page and the previous page's `next` thereafter. Each
    /// call is self-contained (it opens a transaction, reads its page and closes), so no
    /// snapshot is held while a caller decides what to do with the rows.
    ///
    /// Consecutive pages are therefore *not* one snapshot: a write landing between them is
    /// visible to the later page. Bounding the source stack's layers makes the view immutable
    /// and the paging repeatable.
    pub fn flatten_page(
        &self,
        src: &Stack,
        dst: &Stack,
        after: Option<&FlattenCursor>,
        limit: usize,
    ) -> Result<FlattenPage> {
        let mut ctx = self.scan_context(src, dst)?;
        let mut items = Vec::with_capacity(limit.min(1024));
        let next = scan(
            &ctx.txn,
            &ctx.src_layers,
            &mut ctx.dst,
            &ctx.relations,
            after.map(|c| c.0.as_slice()),
            |rel, effect| {
                if limit == 0 {
                    return Ok(ControlFlow::Break(()));
                }
                let relation = rel.name.to_string();
                items.push(match effect {
                    RowEffect::Effective { key, val, asserts } => FlattenItem::Copy {
                        relation,
                        key: decode_tuple_from_key(key, rel.n_keys),
                        value: decode_values(val)?,
                        asserts,
                    },
                    RowEffect::Redundant { key } => FlattenItem::Dedupe {
                        relation,
                        key: decode_tuple_from_key(key, rel.n_keys),
                    },
                    RowEffect::Inert { key } => FlattenItem::TombstoneDropped {
                        relation,
                        key: decode_tuple_from_key(key, rel.n_keys),
                    },
                    RowEffect::Divergent(conflict) => FlattenItem::Conflict(conflict),
                });
                Ok(if items.len() >= limit {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            },
        )?;
        Ok(FlattenPage {
            items,
            next: next.map(FlattenCursor),
        })
    }

    /// Materialize the net effect of `src` into `dst`'s top layer.
    ///
    /// `src` is any windowed sub-stack: an unwindowed single layer is a merge-down, a windowed
    /// one a cherry-pick. `dst`'s lower layers are read for the liveness and identity checks
    /// and never written.
    ///
    /// Rows are decided and written as they stream, so memory is proportional to the number of
    /// conflicts rather than to the size of the view. A collision does not stop the scan: every
    /// one is collected, and the transaction is then dropped without committing, which is what
    /// leaves `dst` byte-identical. [`Db::flatten_plan`] reports the same conflicts without
    /// attempting the write at all.
    ///
    /// `dst`'s top layer may not appear in `src`: the scan reads the source while the write
    /// lands in that layer, and a layer that is both would be read as it is written.
    ///
    /// This specifies no atomicity against concurrent writers on either stack: the caller must
    /// quiesce. The intended pattern, flattening sealed layers into a single-writer head, makes
    /// that free. The commit lock is held for the whole call, not merely the writes, because
    /// the stamp is allocated before the first row is decided.
    pub fn flatten(&self, src: &Stack, dst: &Stack, restamp: bool) -> Result<FlattenStats> {
        let mut ctx = self.scan_context(src, dst)?;
        let top = ctx.dst.layers[0].cf.clone();
        if let Some(clash) = ctx.src_layers.iter().find(|l| l.name == ctx.dst.layers[0].name) {
            bail!(
                "cannot flatten: layer '{}' is both the source of this flatten and the \
                 destination's top layer",
                clash.name
            );
        }

        // The stamp is commit order, so it has to be read where commits are serialized,
        // and the batch has to land before the next commit proceeds.
        let _ordered = self
            .db
            .inner
            .commit_lock
            .lock()
            .map_err(|_| miette!("commit lock poisoned"))?;
        let seq = self.db.inner.next_stamp();

        let mut stats = FlattenStats::default();
        let mut conflicts = vec![];
        let mut failed = None;
        scan(
            &ctx.txn,
            &ctx.src_layers,
            &mut ctx.dst,
            &ctx.relations,
            None,
            |_rel, effect| {
                match effect {
                    RowEffect::Effective { key, val, .. } => {
                        let mut key = key.to_vec();
                        if restamp {
                            // Without this, a later time-travel read of the destination
                            // would report the rows as present at sequences they were not.
                            restamp_tail_validity(&mut key, seq);
                        }
                        stats.rows_copied += 1;
                        stats.bytes_copied += (key.len() + val.len()) as u64;
                        // Writing as we go is safe because each identity is visited once:
                        // no later liveness check can read a row this loop just wrote.
                        if let Err(err) = ctx.txn.put_cf(&top, &key, val) {
                            failed = Some(err);
                            return Ok(ControlFlow::Break(()));
                        }
                    }
                    RowEffect::Redundant { .. } => stats.rows_deduped += 1,
                    RowEffect::Inert { .. } => stats.tombstones_dropped += 1,
                    RowEffect::Divergent(conflict) => conflicts.push(conflict),
                }
                Ok(ControlFlow::Continue(()))
            },
        )?;
        if let Some(err) = failed {
            return Err(err).into_diagnostic().wrap_err("failed to write a flattened row");
        }
        if !conflicts.is_empty() {
            // Dropping the transaction unwritten is what makes a failed flatten a no-op.
            bail!("{}", describe_conflicts(&conflicts));
        }
        if restamp {
            // A flatten is one transaction, so it is one point in commit order.
            stats.seq_range = Some((seq, seq));
        }
        ctx.txn.commit()
            .into_diagnostic()
            .wrap_err("failed to commit the flatten")?;
        Ok(stats)
    }
}

/// Name every collision in an error, capped so that a merge of a large divergent branch does
/// not produce an unreadable message. The full list is what [`Db::flatten_plan`] is for.
fn describe_conflicts(conflicts: &[FlattenConflict]) -> String {
    const SHOWN: usize = 5;
    let mut msg = format!(
        "flatten aborted: {} key(s) are claimed with more than one value",
        conflicts.len()
    );
    for c in conflicts.iter().take(SHOWN) {
        msg.push_str(&format!("\n  {} {:?}:", c.relation, c.key));
        for claim in &c.claims {
            msg.push_str(&format!(" {}={:?}", claim.layer, claim.value));
        }
    }
    if conflicts.len() > SHOWN {
        msg.push_str(&format!(
            "\n  ... and {} more; `flatten_plan` reports every one",
            conflicts.len() - SHOWN
        ));
    }
    msg
}

/// Whether one layer holds any row of a relation, window included.
fn holds_rows(txn: &LayeredTxn<'_>, layer: &BoundLayer<'_>, rel: &RelInfo) -> Result<bool> {
    let lower = rel.id.raw_encode();
    let upper = rel.id.next().raw_encode();
    let mut it = txn.raw_iterator_cf(&layer.cf);
    it.seek(&lower);
    while let Some(k) = it.key() {
        if k >= upper.as_slice() {
            break;
        }
        if layer.window.admits(k) {
            return Ok(true);
        }
        it.next();
    }
    it.status()
        .into_diagnostic()
        .wrap_err("failed to scan a layer")?;
    Ok(false)
}
