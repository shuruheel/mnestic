/*
 * Layered storage: windowed iterators and the k-way stack merge.
 */

use miette::{miette, Result};
use rocksdb::{DBRawIteratorWithThreadMode, MultiThreaded, OptimisticTransactionDB, Transaction};

use std::borrow::Cow;

use crate::data::memcmp::tail_validity;
use crate::data::tuple::{key_ends_in_validity, Tuple};
use crate::runtime::relation::try_decode_tuple_from_kv;
use crate::storage::layered::Seq;
use crate::storage::StoreCursor;

pub(crate) type LayeredDb = OptimisticTransactionDB<MultiThreaded>;
pub(crate) type LayeredTxn<'a> = Transaction<'a, LayeredDb>;
type RawIter<'a> = DBRawIteratorWithThreadMode<'a, LayeredTxn<'a>>;

/// The visibility window of one layer within one stack.
///
/// `since` is an exclusive floor and `bound` an inclusive ceiling, so the window `(fork, head]`
/// is exactly "everything since the fork" with the fork-point row belonging to the parent.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Window {
    /// Rows stamped at or below this sequence are invisible. `None` is an unbounded floor.
    pub since: Option<Seq>,
    /// Rows stamped above this sequence are invisible. `None` is an unbounded ceiling.
    pub bound: Option<Seq>,
}

impl Window {
    pub(crate) const OPEN: Window = Window {
        since: None,
        bound: None,
    };

    /// Whether a row's key falls inside this window.
    ///
    /// A row with no validity carries no sequence, so no window excludes it: a window selects
    /// by *when*, and such a row has no when. The relations this applies to are index
    /// relations (everything else without a validity is refused by a multi-layer stack
    /// altogether), and it is what makes an index compose through a stack at all:
    /// a bounded base layer must keep contributing its edges, or traversal from a branch sees
    /// only the branch's own nodes and silently loses the rest.
    pub(crate) fn admits(&self, key: &[u8]) -> bool {
        if self.since.is_none() && self.bound.is_none() {
            // An open window excludes nothing, so the row's sequence never has to be read.
            // Every layer of a single-layer stack is open, so this is the common path.
            return true;
        }
        let vld = tail_validity(key);
        debug_assert_eq!(
            vld.is_some(),
            key_ends_in_validity(key),
            "the cheap validity probe disagreed with a full decode"
        );
        match vld {
            None => true,
            Some(vld) => {
                let seq = vld.timestamp.0 .0;
                if let Some(since) = self.since {
                    if seq <= since {
                        return false;
                    }
                }
                if let Some(bound) = self.bound {
                    if seq > bound {
                        return false;
                    }
                }
                true
            }
        }
    }
}

/// One layer's contribution to a scan: a raw RocksDB iterator that only ever rests on a row
/// inside the layer's window and below the scan's upper bound.
struct LayerIter<'a> {
    inner: RawIter<'a>,
    window: Window,
    exhausted: bool,
}

impl<'a> LayerIter<'a> {
    fn new(inner: RawIter<'a>, window: Window) -> Self {
        Self {
            inner,
            window,
            exhausted: false,
        }
    }

    fn seek(&mut self, from: &[u8], upper: Option<&[u8]>) -> Result<()> {
        self.exhausted = false;
        self.inner.seek(from);
        self.settle(upper)
    }

    /// Advance past rows that this stack cannot see: those beyond the scan's upper bound, and
    /// those outside the layer's window.
    fn settle(&mut self, upper: Option<&[u8]>) -> Result<()> {
        loop {
            if self.exhausted {
                return Ok(());
            }
            match self.inner.key() {
                None => {
                    self.exhausted = true;
                    return self
                        .inner
                        .status()
                        .map_err(|err| miette!("layer iteration failed: {}", err));
                }
                Some(key) => {
                    if let Some(upper) = upper {
                        if key >= upper {
                            self.exhausted = true;
                            return Ok(());
                        }
                    }
                    if self.window.admits(key) {
                        return Ok(());
                    }
                    self.inner.next();
                }
            }
        }
    }

    fn key(&self) -> Option<&[u8]> {
        if self.exhausted {
            None
        } else {
            self.inner.key()
        }
    }

    fn value(&self) -> Option<&[u8]> {
        if self.exhausted {
            None
        } else {
            self.inner.value()
        }
    }

    fn advance(&mut self, upper: Option<&[u8]>) -> Result<()> {
        if self.exhausted {
            return Ok(());
        }
        self.inner.next();
        self.settle(upper)
    }
}

/// The k-way merge across a stack.
///
/// Entries are emitted in *full-key* order. For a stackable relation the key embeds the
/// validity, which sorts descending, so every version of a relation key arrives newest-first
/// across the whole stack, irrespective of which layer holds it.
///
/// Shadowing between layers is therefore not positional. [`StackCursor`] feeds the
/// ordinary single-store validity rule to that newest-first stream, so the winner is the
/// newest version, whichever layer holds it. Stack position decides only which copy of a
/// byte-identical key is emitted.
///
/// Layers are compared linearly rather than through a heap: a stack is a short-lived divergence
/// merged down promptly, so depth stays in the single digits and a scan of the array
/// beats maintaining a heap.
pub(crate) struct StackMerge<'a> {
    layers: Vec<LayerIter<'a>>,
    upper: Option<Cow<'a, [u8]>>,
    /// The key the merge is currently sitting on, so that every layer holding it can be
    /// advanced once the row has been handed out. Reused between rows.
    front_key: Vec<u8>,
    /// Whether a row has been handed out and not yet advanced past.
    positioned: bool,
}

impl<'a> StackMerge<'a> {
    pub(crate) fn new(iters: Vec<(RawIter<'a>, Window)>, upper: Option<Cow<'a, [u8]>>) -> Self {
        Self {
            layers: iters
                .into_iter()
                .map(|(it, win)| LayerIter::new(it, win))
                .collect(),
            upper,
            front_key: vec![],
            positioned: false,
        }
    }

    pub(crate) fn seek(&mut self, from: &[u8]) -> Result<()> {
        // Seeking repositions every layer, so there is nothing left to advance past.
        self.positioned = false;
        let upper = self.upper.as_deref();
        for layer in self.layers.iter_mut() {
            layer.seek(from, upper)?;
        }
        Ok(())
    }

    /// The index of the layer holding the smallest key, preferring the topmost on a tie.
    fn front(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (idx, layer) in self.layers.iter().enumerate() {
            let Some(key) = layer.key() else { continue };
            match best {
                None => best = Some(idx),
                // Strictly less, so a tie leaves the slot with the earlier (higher) layer.
                // A tie is byte-identical keys (same relation key *and* same stamp), so this
                // chooses which copy of one row to emit. It does not decide which version of a
                // key wins; the stamp does, in the validity scan above.
                Some(b) => {
                    if key < self.layers[b].key().unwrap() {
                        best = Some(idx)
                    }
                }
            }
        }
        best
    }

    /// The next row, borrowed from the layer holding it, with that layer's index.
    ///
    /// The index is returned alongside rather than through an accessor because the row borrows
    /// the merge, so nothing else can be asked of it while the row is in hand.
    ///
    /// The borrow lasts until the next call, which is what the `&mut self` receiver enforces:
    /// advancing invalidates the underlying iterator's buffer, so nothing may outlive it. A
    /// caller that needs to keep a row copies it, and one that only inspects it does not.
    ///
    /// Advancing happens at the start of the following call rather than before returning,
    /// which is what lets this stay a single call per row.
    pub(crate) fn next_borrowed(&mut self) -> Option<Result<(usize, &[u8], &[u8])>> {
        if self.positioned {
            if let Err(err) = self.advance_front() {
                return Some(Err(err));
            }
        }
        let front = self.front()?;
        // Record the key before handing out borrows: the advance needs it to recognise every
        // layer sitting on this row, and by then the row itself is gone.
        self.front_key.clear();
        self.front_key
            .extend_from_slice(self.layers[front].key().unwrap());
        self.positioned = true;
        let layer = &self.layers[front];
        Some(Ok((
            front,
            layer.key().unwrap(),
            layer.value().unwrap_or_default(),
        )))
    }

    /// Advance every layer sitting on the row just handed out, not only the one it came from:
    /// that is what collapses a row present identically in several layers into one emission.
    fn advance_front(&mut self) -> Result<()> {
        self.positioned = false;
        let Self {
            layers,
            upper,
            front_key,
            ..
        } = self;
        let upper = upper.as_deref();
        for layer in layers.iter_mut() {
            if layer.key() == Some(front_key.as_slice()) {
                layer.advance(upper)?;
            }
        }
        Ok(())
    }
}

/// Raw `(key, value)` scan across a stack.
pub(crate) struct StackRawIter<'a> {
    pub(crate) merge: StackMerge<'a>,
    pub(crate) started: bool,
    pub(crate) lower: Vec<u8>,
}

impl<'a> StackRawIter<'a> {
    /// The next row, borrowed. Performs the initial seek on first use.
    pub(crate) fn next_borrowed(&mut self) -> Option<Result<(&[u8], &[u8])>> {
        if !self.started {
            self.started = true;
            if let Err(err) = self.merge.seek(&self.lower) {
                return Some(Err(err));
            }
        }
        match self.merge.next_borrowed()? {
            Ok((_, key, val)) => Some(Ok((key, val))),
            Err(err) => Some(Err(err)),
        }
    }
}

impl<'a> Iterator for StackRawIter<'a> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.started {
            self.started = true;
            if let Err(err) = self.merge.seek(&self.lower) {
                return Some(Err(err));
            }
        }
        match self.next_borrowed()? {
            // The one place a copy is unavoidable: `Iterator::Item` cannot borrow from the
            // iterator, and `StoreTx::range_scan` is declared to yield owned pairs.
            Ok((key, val)) => Some(Ok((key.to_vec(), val.to_vec()))),
            Err(err) => Some(Err(err)),
        }
    }
}

/// Decoded-tuple scan across a stack.
pub(crate) struct StackTupleIter<'a> {
    pub(crate) inner: StackRawIter<'a>,
}

impl<'a> Iterator for StackTupleIter<'a> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        // Decoding reads through the borrows, so the row is never copied. A value blob that
        // will not decode is an error, never a panic: a corrupt row must leave the rest of the
        // history readable.
        match self.inner.next_borrowed() {
            Some(Ok((k, v))) => Some(try_decode_tuple_from_kv(k, v, None)),
            Some(Err(err)) => Some(Err(err)),
            None => None,
        }
    }
}

/// Frontier scan across a stack: for each key, the newest version at or before `valid_at`,
/// skipped entirely when that version is a retraction.
///
/// The merge below it already presents each key's versions newest-first across all layers, so
/// cross-layer shadowing and cross-layer retraction both fall out of running the
/// single-store validity rule over the merged stream.
/// A positioned scan over a layer stack.
///
/// The merge is already a lending cursor, so a row the scan declines is never copied: only the
/// index of the layer holding it is kept, and the key and value stay where the iterators put
/// them.
pub(crate) struct StackCursor<'a> {
    pub(crate) merge: StackMerge<'a>,
    front: Option<usize>,
}

impl<'a> StackCursor<'a> {
    pub(crate) fn new(merge: StackMerge<'a>) -> Self {
        StackCursor { merge, front: None }
    }

    fn front(&self) -> &LayerIter<'a> {
        &self.merge.layers[self.front.expect("cursor read after a successful seek")]
    }
}

impl StoreCursor for StackCursor<'_> {
    fn seek(&mut self, from: &[u8]) -> Result<bool> {
        self.merge.seek(from)?;
        self.front = match self.merge.next_borrowed() {
            None => None,
            Some(Err(err)) => return Err(err),
            Some(Ok((front, _, _))) => Some(front),
        };
        Ok(self.front.is_some())
    }

    fn key(&self) -> &[u8] {
        self.front().key().expect("a positioned layer has a key")
    }

    fn value(&mut self) -> Result<&[u8]> {
        Ok(self.front().value().unwrap_or_default())
    }
}
