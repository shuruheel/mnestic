/*
 * Layered storage: the stack-aware transaction.
 */

use std::cell::OnceCell;
use std::sync::Arc;

use miette::{bail, miette, IntoDiagnostic, Result, WrapErr};
use rocksdb::BoundColumnFamily;

use std::borrow::Cow;

use crate::data::memcmp::{
    contains_validity_ts, restamp_all_validity, tail_validity, KeyBounds,
};
use crate::storage::layered::catalog::{catalog_relations, relation_of, Catalog};
use crate::data::tuple::Tuple;
use crate::storage::layered::iter::{
    LayeredTxn, StackCursor, StackMerge, StackRawIter, StackTupleIter, Window,
};
use crate::storage::layered::{LayeredInner, StackSpec, Seq, PENDING_SEQ};
use crate::storage::{StorageView, StoreCursor, StoreTx};

/// A resolved stack, together with the scratch space its per-key lookups reuse.
///
/// The buffers are an implementation detail of [`BoundStack::key_state`]: they are filled and
/// consumed inside a single call, so nothing outside this type sees them.
pub(crate) struct BoundStack<'a> {
    pub(crate) layers: Vec<BoundLayer<'a>>,
    /// The identity of the stack these layers came from, carried so a transaction can report
    /// which view it reads without re-deriving it.
    pub(crate) view: StorageView,
    bounds: KeyBounds,
}

impl<'a> BoundStack<'a> {
    pub(crate) fn new(layers: Vec<BoundLayer<'a>>, view: StorageView) -> Self {
        Self {
            layers,
            view,
            bounds: KeyBounds::default(),
        }
    }

    /// The exclusive end of the range covering every version of `key`'s identity, held in
    /// this stack's own buffer.
    ///
    /// The value is only valid until the next call that fills the buffer, which is why it is
    /// returned as a borrow rather than handed out.
    pub(crate) fn range_end(&mut self, key: &[u8]) -> &[u8] {
        self.bounds.fill(key);
        self.bounds.upper()
    }

    /// What this stack says about one key's identity right now.
    ///
    /// Reads the identity's versions newest first and stops at the first assertion, which is
    /// all either answer needs: the newest version decides liveness, and under value
    /// immutability every assertion under a key carries the same value. Nothing beyond that
    /// point is read, so a key with a long history costs no more than one with two versions.
    pub(crate) fn key_state(&mut self, txn: &LayeredTxn<'_>, key: &[u8]) -> Result<KeyState> {
        self.bounds.fill(key);
        let iters = self
            .layers
            .iter()
            .map(|l| (txn.raw_iterator_cf(&l.cf), l.window))
            .collect();
        let mut merge = StackMerge::new(iters, Some(Cow::Borrowed(self.bounds.upper())));
        merge.seek(self.bounds.lower())?;

        let mut state = KeyState {
            live: false,
            asserted: None,
            asserted_layer: None,
        };
        let mut idx = 0usize;
        while let Some(row) = merge.next_borrowed() {
            let (at_layer, k, v) = row?;
            let at = idx;
            idx += 1;
            let Some(vld) = tail_validity(k) else { continue };
            if at == 0 {
                state.live = vld.is_assert.0;
            }
            if vld.is_assert.0 {
                // The one value worth keeping; every tombstone before it is only read.
                state.asserted = Some(v.to_vec());
                state.asserted_layer = Some(at_layer);
                break;
            }
        }
        Ok(state)
    }
}

/// A layer, resolved against the open database for the life of one transaction.
pub(crate) struct BoundLayer<'a> {
    pub(crate) name: String,
    /// The incarnation the name resolved to; part of this stack's view identity.
    pub(crate) incarnation: u64,
    pub(crate) cf: Arc<BoundColumnFamily<'a>>,
    pub(crate) window: Window,
}

/// A transaction over a stack of layers.
///
/// Reads compose the stack; writes land in the top layer only, which is what makes concurrent
/// stacks safe. Catalog keys are the exception: they are global and are
/// read and written against the default column family whatever stack is in play.
pub struct LayeredTx<'a> {
    pub(crate) inner: &'a LayeredInner,
    pub(crate) tx: Option<LayeredTxn<'a>>,
    pub(crate) stack: BoundStack<'a>,
    pub(crate) catalog: Arc<BoundColumnFamily<'a>>,
    /// Keys written with the pending stamp, awaiting a sequence number at commit.
    pub(crate) pending: Vec<Vec<u8>>,
    /// The catalog, read lazily: only a multi-layer stack needs it, and then only once.
    pub(crate) relations: OnceCell<Catalog>,
}

// Same caveat as the other RocksDB backends: the underlying transaction is not `Sync`, and the
// engine relies on Cozo never sharing one transaction across threads concurrently.
unsafe impl<'a> Sync for LayeredTx<'a> {}

/// Catalog keys live under the system relation, which is global across layers.
#[inline]
pub(crate) fn is_catalog_key(key: &[u8]) -> bool {
    key.len() >= 8 && key[..8] == [0u8; 8]
}

impl<'a> LayeredTx<'a> {
    fn txn(&self) -> Result<&LayeredTxn<'a>> {
        self.tx
            .as_ref()
            .ok_or_else(|| miette!("transaction already committed"))
    }

    fn top(&self) -> &BoundLayer<'a> {
        &self.stack.layers[0]
    }

    /// Where a write goes: the catalog is global, everything else goes to the top layer.
    fn write_target(&self, key: &[u8]) -> Arc<BoundColumnFamily<'a>> {
        if is_catalog_key(key) {
            self.catalog.clone()
        } else {
            self.top().cf.clone()
        }
    }

    /// The layers a scan of `lower` must compose. Catalog scans see the default layer alone.
    fn read_layers(&self, lower: &[u8]) -> Vec<(Arc<BoundColumnFamily<'a>>, Window)> {
        if is_catalog_key(lower) {
            vec![(self.catalog.clone(), Window::OPEN)]
        } else {
            self.stack.layers
                .iter()
                .map(|l| (l.cf.clone(), l.window))
                .collect()
        }
    }

    fn merge(&'a self, lower: &[u8], upper: Option<Vec<u8>>) -> Result<StackMerge<'a>> {
        let txn = self.txn()?;
        let iters = self
            .read_layers(lower)
            .into_iter()
            .map(|(cf, win)| (txn.raw_iterator_cf(&cf), win))
            .collect();
        Ok(StackMerge::new(iters, upper.map(std::borrow::Cow::Owned)))
    }

    fn raw_iter(&'a self, lower: &[u8], upper: Option<Vec<u8>>) -> Result<StackRawIter<'a>> {
        Ok(StackRawIter {
            merge: self.merge(lower, upper)?,
            started: false,
            lower: lower.to_vec(),
        })
    }

    /// What the catalog says about every relation, read once per transaction and only when a
    /// multi-layer stack actually needs it.
    fn relations(&self) -> Result<&Catalog> {
        if let Some(known) = self.relations.get() {
            return Ok(known);
        }
        let scanned = catalog_relations(self.txn()?, &self.catalog)?;
        Ok(self.relations.get_or_init(|| scanned))
    }

    /// The gate a multi-layer stack puts in front of every key it touches.
    ///
    /// Stackability is decided at relation creation and read back from the catalog here, so
    /// the answer depends on the relation alone, not on which layer a particular row happens
    /// to sit in, and not on whether the operation would have found anything. It fails before
    /// any row is read, which is the whole point of checking it here rather than at delete time.
    fn gate(&self, key: &[u8], destructive: bool) -> Result<()> {
        if self.stack.layers.len() < 2 {
            return Ok(());
        }
        if is_catalog_key(key) {
            if destructive {
                // Destructive DDL through a multi-layer stack would strike layers the caller is
                // not writing to: the catalog is global, but the rows are not.
                bail!(
                    "destructive schema changes need a single-layer stack; this one is [{}]",
                    self.stack_description()
                );
            }
            return Ok(());
        }
        let Some(rel_id) = relation_of(key) else {
            return Ok(());
        };
        let Some(info) = self.relations()?.get(rel_id) else {
            // A relation the catalog has not caught up with: a definition written earlier in
            // this same transaction. It cannot be older than this stack, so let it through.
            return Ok(());
        };
        // Index relations are exempt. They hold no user records (nothing anyone retracts),
        // and the engine maintains them inside whichever layer is being written. Reads compose
        // through the stack by exact key, and a stack topology change invalidates them anyway:
        // the remedy is drop-and-rebuild, not a retraction.
        if !info.stackable && !info.is_index {
            bail!(
                "relation '{}' has no validity column, so it cannot express a cross-layer \
                 retraction and may only be reached through a single-layer stack; this one is [{}]",
                info.name,
                self.stack_description()
            );
        }
        if destructive && !info.is_index {
            // A hard delete can only remove a row from the layer it is written to, so through a
            // stack it is never the operation the caller wants. Index rows are the exception:
            // they are maintained by the engine within the layer that wrote them.
            bail!(
                "relation '{}' cannot be deleted from through stack [{}]: a delete that must \
                 cross layers is a retraction",
                info.name,
                self.stack_description()
            );
        }
        Ok(())
    }

    /// The layer stack, as spelled for error messages.
    pub(crate) fn stack_description(&self) -> String {
        self.stack.layers
            .iter()
            .map(|l| match (l.window.since, l.window.bound) {
                (None, None) => l.name.clone(),
                (None, Some(b)) => format!("{}@{}", l.name, b),
                (Some(s), None) => format!("{}({}..]", l.name, s),
                (Some(s), Some(b)) => format!("{}({}..{}]", l.name, s, b),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// What a stack currently says about one key's identity: everything a write-path or flatten
/// decision needs.
pub(crate) struct KeyState {
    /// Whether the newest visible version asserts.
    pub(crate) live: bool,
    /// The value of the newest visible assertion, if the key was ever asserted. Under value
    /// immutability every assertion under a key carries this same value.
    pub(crate) asserted: Option<Vec<u8>>,
    /// Which layer of the stack holds `asserted`, as an index into its layers.
    pub(crate) asserted_layer: Option<usize>,
}

impl<'s> StoreTx<'s> for LayeredTx<'s> {
    fn get(&self, key: &[u8], _for_update: bool) -> Result<Option<Vec<u8>>> {
        self.gate(key, false)?;
        let txn = self.txn()?;
        if is_catalog_key(key) {
            return txn
                .get_cf(&self.catalog, key)
                .into_diagnostic()
                .wrap_err("failed to read catalog");
        }
        for layer in self.stack.layers.iter() {
            // A key invisible through this layer's window contributes nothing here, but a
            // lower layer with a different window may still hold it.
            if !layer.window.admits(key) {
                continue;
            }
            let found = txn
                .get_cf(&layer.cf, key)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read layer {}", layer.name))?;
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    /// Resolve many keys at once: one batched read per layer rather than one point lookup per
    /// key per layer.
    ///
    /// Layers are visited top first and a slot is filled only once, so the result is the same
    /// as calling [`StoreTx::get`] on each key, which is what the default implementation does.
    /// Which view this transaction reads, decided when the stack was resolved.
    ///
    /// Interning happens in `resolve`, which already holds the layer registry, so this is a
    /// field read: the identity costs nothing to hand out and nothing to compare.
    fn storage_view(&self) -> StorageView {
        self.stack.view.clone()
    }

    fn multi_get(&self, keys: &[Vec<u8>], _for_update: bool) -> Result<Vec<Option<Vec<u8>>>> {
        for key in keys {
            self.gate(key, false)?;
        }
        let txn = self.txn()?;
        let mut out: Vec<Option<Vec<u8>>> = vec![None; keys.len()];

        // The catalog is global, so its keys are resolved against the default layer whatever
        // stack is in play.
        let catalog_at: Vec<usize> = (0..keys.len())
            .filter(|&i| is_catalog_key(&keys[i]))
            .collect();
        if !catalog_at.is_empty() {
            let found = txn.multi_get_cf(
                catalog_at
                    .iter()
                    .map(|&i| (&self.catalog, keys[i].as_slice())),
            );
            for (&at, res) in catalog_at.iter().zip(found) {
                out[at] = res.into_diagnostic().wrap_err("failed to read catalog")?;
            }
        }

        let mut pending: Vec<usize> = (0..keys.len())
            .filter(|&i| !is_catalog_key(&keys[i]) )
            .collect();
        for layer in self.stack.layers.iter() {
            if pending.is_empty() {
                break;
            }
            // A key this layer's window excludes contributes nothing here, but a lower layer
            // with a different window may still hold it.
            let asking: Vec<usize> = pending
                .iter()
                .copied()
                .filter(|&i| layer.window.admits(&keys[i]))
                .collect();
            if asking.is_empty() {
                continue;
            }
            let found = txn.multi_get_cf(asking.iter().map(|&i| (&layer.cf, keys[i].as_slice())));
            for (&at, res) in asking.iter().zip(found) {
                if let Some(val) = res
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to read layer {}", layer.name))?
                {
                    out[at] = Some(val);
                }
            }
            pending.retain(|&i| out[i].is_none());
        }
        Ok(out)
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.gate(key, false)?;
        let target = self.write_target(key);
        let vld = tail_validity(key);
        let stamped = matches!(vld, Some(v) if v.timestamp.0 .0 == PENDING_SEQ);
        // A derived row, such as an index entry, carries the stamped key of the row it describes
        // rather than referring to it, so the pending stamp can be anywhere in either half.
        let carries_pending =
            stamped || contains_validity_ts(key, PENDING_SEQ) || contains_validity_ts(val, PENDING_SEQ);

        // The sequence is assigned by storage, not by the query. A user-supplied value above
        // the current sequence would shadow future writes and break layer isolation, and one
        // below it would appear to predate a fork it postdates. Failing is better than
        // accepting it or silently ignoring it.
        if vld.is_some() && !stamped {
            bail!(
                "explicit validity is not accepted: the sequence is assigned by storage at \
                 commit. Use 'ASSERT' or 'RETRACT'."
            );
        }

        // A key's value is immutable; its visibility is not. Re-asserting the
        // identical value is how a retracted record is re-introduced; asserting a different
        // one (over a live row or a tombstoned one) is backdoor mutability, and merge
        // soundness rests on it never happening.
        if stamped && vld.map_or(false, |v| v.is_assert.0) {
            // Borrow the transaction and the stack disjointly: the lookup needs the stack
            // mutably, and the transaction is a sibling field.
            let Self { tx, stack, .. } = self;
            let txn = tx
                .as_ref()
                .ok_or_else(|| miette!("transaction already committed"))?;
            let state = stack.key_state(txn, key)?;
            if let Some(previous) = state.asserted {
                if previous != val {
                    bail!(
                        "a different value is already asserted under this key through stack \
                         [{}]: a record may be created and retracted, never updated",
                        self.stack_description()
                    );
                }
            }
        }

        let txn = self.txn()?;
        txn.put_cf(&target, key, val)
            .into_diagnostic()
            .wrap_err("failed to write row")?;
        if carries_pending {
            self.pending.push(key.to_vec());
        }
        Ok(())
    }

    fn supports_par_put(&self) -> bool {
        // Deliberately not supported. Parallel puts would have several threads writing to one
        // RocksDB transaction at once, and a transaction's write batch is not thread-safe; the
        // pending-stamp buffer and the write-path invariant check would
        // both need locking on top of that. Cozo falls back to sequential puts, which costs
        // bulk-import throughput and nothing else.
        false
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        self.gate(key, true)?;
        let target = self.write_target(key);
        let txn = self.txn()?;
        txn.delete_cf(&target, key)
            .into_diagnostic()
            .wrap_err("failed to delete row")
    }

    fn del_range_from_persisted(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        self.gate(lower, true)?;
        // Deletes only ever touch the layer being written to, so this is a top-layer scan.
        let target = self.write_target(lower);
        let txn = self.txn()?;
        let mut it = txn.raw_iterator_cf(&target);
        it.seek(lower);
        let mut doomed = vec![];
        while let Some(key) = it.key() {
            if key >= upper {
                break;
            }
            doomed.push(key.to_vec());
            it.next();
        }
        it.status()
            .into_diagnostic()
            .wrap_err("failed to scan for range delete")?;
        for key in doomed {
            txn.delete_cf(&target, &key)
                .into_diagnostic()
                .wrap_err("failed during range delete")?;
        }
        Ok(())
    }

    fn exists(&self, key: &[u8], for_update: bool) -> Result<bool> {
        Ok(self.get(key, for_update)?.is_some())
    }

    fn commit(&mut self) -> Result<()> {
        let txn = self
            .tx
            .take()
            .ok_or_else(|| miette!("transaction already committed"))?;

        // The stamp is commit order, so it has to be read where commits are serialized, and the
        // batch has to land before the next commit proceeds.
        let _ordered = self
            .inner
            .commit_lock
            .lock()
            .map_err(|_| miette!("commit lock poisoned"))?;

        if !self.pending.is_empty() {
            let seq = self.inner.next_stamp();
            let cf = self.top().cf.clone();
            for key in std::mem::take(&mut self.pending) {
                // A row written and then deleted within this transaction has nothing to stamp.
                let Some(mut val) = txn.get_cf(&cf, &key).into_diagnostic()? else {
                    continue;
                };
                let mut stamped = key.clone();
                restamp_all_validity(&mut stamped, PENDING_SEQ, seq);
                restamp_all_validity(&mut val, PENDING_SEQ, seq);
                txn.delete_cf(&cf, &key).into_diagnostic()?;
                txn.put_cf(&cf, &stamped, &val).into_diagnostic()?;
            }
        }

        txn.commit().into_diagnostic().wrap_err("commit failed")
    }

    fn range_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        match self.gate(lower, false).and_then(|()| self.raw_iter(lower, Some(upper.to_vec()))) {
            Ok(inner) => Box::new(StackTupleIter { inner }),
            Err(err) => Box::new(std::iter::once(Err(err))),
        }
    }

    fn cursor<'a>(&'a self, lower: &[u8], upper: &[u8]) -> Result<Box<dyn StoreCursor + 'a>>
    where
        's: 'a,
    {
        self.gate(lower, false)?;
        Ok(Box::new(StackCursor::new(
            self.merge(lower, Some(upper.to_vec()))?,
        )))
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        match self.gate(lower, false).and_then(|()| self.raw_iter(lower, Some(upper.to_vec()))) {
            Ok(it) => Box::new(it),
            Err(err) => Box::new(std::iter::once(Err(err))),
        }
    }

    fn range_count<'a>(&'a self, lower: &[u8], upper: &[u8]) -> Result<usize>
    where
        's: 'a,
    {
        self.gate(lower, false)?;
        let mut count = 0;
        for row in self.raw_iter(lower, Some(upper.to_vec()))? {
            row?;
            count += 1;
        }
        Ok(count)
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        // A total scan is over user data, which lives in the stack; the catalog rides along in
        // the default layer, exactly as it does for a single-store engine.
        match self.raw_iter(&[], None) {
            Ok(it) => Box::new(it),
            Err(err) => Box::new(std::iter::once(Err(err))),
        }
    }
}

/// The sequence a stamped row would carry if it were committed right now. Only meaningful
/// under the commit lock.
impl LayeredInner {
    pub(crate) fn next_stamp(&self) -> Seq {
        // `latest_sequence_number` is the last sequence RocksDB assigned, which is exactly the
        // fork point a consumer would have captured. Stamping one above it is what
        // makes a fork point stable: rows committed after a fork are strictly above it.
        self.db.latest_sequence_number() as Seq + 1
    }
}

/// Resolve a stack spec against the open database, for the life of one transaction.
pub(crate) fn bind_layers<'a>(
    inner: &'a LayeredInner,
    spec: &StackSpec,
) -> Result<Vec<BoundLayer<'a>>> {
    spec.layers
        .iter()
        .map(|layer| {
            let cf = inner.db.cf_handle(&layer.name).ok_or_else(|| {
                miette!(
                    "layer '{}' is not open; it was dropped or never created",
                    layer.name
                )
            })?;
            Ok(BoundLayer {
                name: layer.name.clone(),
                incarnation: layer.incarnation,
                cf,
                window: layer.window,
            })
        })
        .collect()
}
