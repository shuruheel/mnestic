/*
 * Layered storage for the RocksDB backend.
 *
 * A layer is one column family; a stack is an ordered list of layers with visibility windows.
 * Reads compose the stack, writes land in its top layer.
 */

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::Path;
use std::sync::{Mutex, RwLock};

use miette::{bail, miette, IntoDiagnostic, Result, WrapErr};
use rocksdb::{MultiThreaded, OptimisticTransactionDB, Options, WriteBatchWithTransaction, DB};

use crate::data::value::{DataValue, ValidityTs};
use crate::parse::parse_script;
use crate::runtime::db::{BadDbInit, DbManifest, ScriptMutability};
use crate::storage::layered::iter::{LayeredDb, Window};
use crate::storage::layered::tx::{bind_layers, is_catalog_key, LayeredTx};
use crate::storage::{Storage, StorageView};
use crate::{Db, NamedRows};

pub(crate) mod catalog;
pub(crate) mod flatten;
pub(crate) mod iter;
pub(crate) mod tx;

#[cfg(test)]
mod tests;

/// A storage sequence number. This is RocksDB's own commit-order counter, reinterpreted as
/// Cozo's validity timestamp. It is not a wall clock, and it is sparse.
pub type Seq = i64;

/// The stamp a row carries between `put` and `commit`, before commit order is known.
///
/// It sorts newer than any real sequence, so a transaction reads its own buffered writes.
pub const PENDING_SEQ: Seq = i64::MAX - 1;

/// The layer every store starts with. It is a normal layer in every respect except that it
/// cannot be dropped: the catalog and the existing entry points live on it.
pub const DEFAULT_LAYER: &str = "default";

/// The name of a layer. Opaque to Cozo: whether it is a branch, a tenant, a draft or a sandbox
/// is the consumer's business.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct LayerId(pub String);

impl From<&str> for LayerId {
    fn from(s: &str) -> Self {
        LayerId(s.to_string())
    }
}

impl From<String> for LayerId {
    fn from(s: String) -> Self {
        LayerId(s)
    }
}

impl Display for LayerId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A layer as it appears in one stack: which layer, and through what window.
///
/// Windows are per-stack, not per-layer: the same layer may appear in two stacks at different
/// windows simultaneously.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayerRef {
    /// Which layer.
    pub id: LayerId,
    /// Rows stamped at or below this sequence are invisible. `None` for a normal read stack;
    /// a floor is for changeset views, which answer "what changed", not "what is".
    pub since: Option<Seq>,
    /// Rows stamped above this sequence are invisible. `None` leaves the layer unbounded.
    pub bound: Option<Seq>,
}

impl LayerRef {
    /// The whole layer, unwindowed.
    pub fn new(id: impl Into<LayerId>) -> Self {
        Self {
            id: id.into(),
            since: None,
            bound: None,
        }
    }

    /// The layer as of `bound`, a fork point.
    pub fn bounded(id: impl Into<LayerId>, bound: Seq) -> Self {
        Self {
            id: id.into(),
            since: None,
            bound: Some(bound),
        }
    }

    /// The layer's changeset over `(since, bound]`.
    pub fn windowed(id: impl Into<LayerId>, since: Option<Seq>, bound: Option<Seq>) -> Self {
        Self {
            id: id.into(),
            since,
            bound,
        }
    }
}

/// An ordered list of layers. Index 0 is the top, and the only one written to.
pub type Stack = Vec<LayerRef>;

/// This engine's identity for a resolved stack: the interned number the stack was assigned.
///
/// Opaque to everything above: nothing outside decides what makes two stacks the same, and the
/// number itself never escapes.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct StackView(u64);

impl crate::storage::ViewIdentity for StackView {
    fn view_cmp(&self, other: &dyn crate::storage::ViewIdentity) -> Option<std::cmp::Ordering> {
        (other as &dyn std::any::Any)
            .downcast_ref::<StackView>()
            .map(|other| self.0.cmp(&other.0))
    }
}

/// A stack that has been checked against the open store: layers exist, windows are coherent.
#[derive(Clone, Debug, Default)]
pub(crate) struct StackSpec {
    pub(crate) layers: Vec<LayerSpec>,
    /// Which view this stack is, for caches that must not serve one stack's work to another.
    pub(crate) view: StorageView,
}

#[derive(Clone, Debug)]
pub(crate) struct LayerSpec {
    pub(crate) name: String,
    /// The incarnation this name resolved to, so a stack resolved before a drop-and-recreate
    /// is not mistaken for one resolved after it.
    pub(crate) incarnation: u64,
    pub(crate) window: Window,
}

impl StackSpec {
    /// The store read whole: one layer, no window. Every other engine presents exactly this,
    /// so it reports [`StorageView::undivided`] rather than a view of its own.
    fn single(name: &str) -> Self {
        Self {
            layers: vec![LayerSpec {
                name: name.to_string(),
                incarnation: 0,
                window: Window::OPEN,
            }],
            view: StorageView::undivided(),
        }
    }
}

const CURRENT_STORAGE_VERSION: u64 = 3;

/// State shared by every handle onto one layered store.
pub struct LayeredInner {
    pub(crate) db: LayeredDb,
    /// Serializes commits so that the sequence read at commit really is commit order.
    pub(crate) commit_lock: Mutex<()>,
    /// Every layer that exists, with the incarnation it was opened or created at. A name is
    /// reusable after a drop, so the incarnation is what distinguishes the new layer from the
    /// one it replaced for anything that caches per view.
    pub(crate) layers: RwLock<BTreeMap<String, u64>>,
    /// Mints layer incarnations, distinct for every layer this process ever opens or creates.
    /// In-process uniqueness is the requirement: what reads it is in-memory caching, which does
    /// not outlive the process.
    pub(crate) layer_incarnation: std::sync::atomic::AtomicU64,
    /// Every distinct stack this store has resolved, and the opaque view identity it was given.
    /// Interned rather than rendered: engine-level caches only ever compare these, so a number
    /// is both cheaper than a name and impossible to forge by naming a layer after one.
    pub(crate) views: RwLock<BTreeMap<Vec<(u64, Option<Seq>, Option<Seq>)>, u64>>,
}

/// The layered RocksDB storage engine.
///
/// A handle carries the stack it reads and writes through. Handles are cheap and independent:
/// [`Db::run_on_stack`] makes one per call rather than keeping an ambient "current layer",
/// because ambient state plus concurrent writers is a race.
#[derive(Clone)]
pub struct LayeredStorage {
    inner: std::sync::Arc<LayeredInner>,
    stack: std::sync::Arc<StackSpec>,
}

/// Open a layered RocksDB store.
pub fn new_cozo_layered(path: impl AsRef<Path>) -> Result<Db<LayeredStorage>> {
    fs::create_dir_all(&path).map_err(|err| {
        BadDbInit(format!(
            "cannot create directory {}: {}",
            path.as_ref().display(),
            err
        ))
    })?;
    let path_buf = path.as_ref().to_path_buf();

    let manifest_path = path_buf.join("manifest");
    if manifest_path.exists() {
        let manifest_bytes = fs::read(&manifest_path)
            .into_diagnostic()
            .wrap_err("failed to read manifest")?;
        let existing: DbManifest = rmp_serde::from_slice(&manifest_bytes)
            .into_diagnostic()
            .wrap_err("failed to parse manifest")?;
        if existing.storage_version != CURRENT_STORAGE_VERSION {
            bail!(
                "Unsupported storage version {}",
                existing.storage_version
            );
        }
    } else {
        let manifest = DbManifest {
            storage_version: CURRENT_STORAGE_VERSION,
        };
        let manifest_bytes = rmp_serde::to_vec_named(&manifest)
            .into_diagnostic()
            .wrap_err("failed to serialize manifest")?;
        fs::write(&manifest_path, &manifest_bytes)
            .into_diagnostic()
            .wrap_err("failed to write manifest")?;
    }

    let store_path = path_buf.join("data");
    let store_path_str = store_path.to_str().ok_or_else(|| miette!("bad path name"))?;

    // Every column family must be opened for its layer to be usable; enumerate what is on disk.
    let existing_cfs = DB::list_cf(&Options::default(), store_path_str)
        .unwrap_or_else(|_| vec![DEFAULT_LAYER.to_string()]);

    let existing_len = existing_cfs.len();

    let mut options = Options::default();
    options.create_if_missing(true);
    options.create_missing_column_families(true);

    let db = OptimisticTransactionDB::<MultiThreaded>::open_cf(
        &options,
        store_path_str,
        existing_cfs.iter(),
    )
    .into_diagnostic()
    .wrap_err("failed to open RocksDB")?;

    let storage = LayeredStorage {
        inner: std::sync::Arc::new(LayeredInner {
            db,
            commit_lock: Mutex::new(()),
            // Every layer gets a distinct incarnation, the ones already on disk included: a
            // shared value would make two layers indistinguishable to anything keyed on it.
            layers: RwLock::new(
                existing_cfs
                    .into_iter()
                    .enumerate()
                    .map(|(at, name)| (name, at as u64))
                    .collect(),
            ),
            layer_incarnation: std::sync::atomic::AtomicU64::new(existing_len as u64),
            views: RwLock::new(BTreeMap::new()),
        }),
        stack: std::sync::Arc::new(StackSpec::single(DEFAULT_LAYER)),
    };

    let ret = Db::new(storage)?;
    ret.initialize()?;
    Ok(ret)
}

impl LayeredStorage {
    /// The same store, read and written through a different stack.
    pub(crate) fn with_stack(&self, stack: StackSpec) -> Self {
        Self {
            inner: self.inner.clone(),
            stack: std::sync::Arc::new(stack),
        }
    }

    /// Check a stack against the open store. Errors here are cheap and deterministic, which is
    /// why the checks live at stack construction rather than mid-query.
    /// The identity of a resolved stack, interned so that two resolutions of the same stack
    /// compare equal and no two different stacks do.
    ///
    /// A lone unwindowed default layer is the store read whole, which is what every other
    /// engine presents, so it is undivided and shares cache entries with the ordinary path.
    fn view_of(&self, layers: &[LayerSpec]) -> StorageView {
        if let [only] = layers {
            if only.name == DEFAULT_LAYER && only.window == Window::OPEN {
                return StorageView::undivided();
            }
        }
        let shape: Vec<(u64, Option<Seq>, Option<Seq>)> = layers
            .iter()
            .map(|l| (l.incarnation, l.window.since, l.window.bound))
            .collect();
        if let Some(&id) = self.inner.views.read().unwrap().get(&shape) {
            return StorageView::of(std::sync::Arc::new(StackView(id)));
        }
        let mut views = self.inner.views.write().unwrap();
        // Another writer may have interned this shape while the read lock was released, so take
        // whatever is there rather than minting a second number for one stack.
        let next = views.len() as u64;
        let id = *views.entry(shape).or_insert(next);
        StorageView::of(std::sync::Arc::new(StackView(id)))
    }

    pub(crate) fn resolve(&self, stack: &Stack) -> Result<StackSpec> {
        if stack.is_empty() {
            bail!("a stack must name at least one layer");
        }
        let known = self.inner.layers.read().unwrap();
        let mut seen = BTreeSet::new();
        let mut layers = Vec::with_capacity(stack.len());
        for layer in stack {
            let name = layer.id.0.clone();
            let Some(&incarnation) = known.get(&name) else {
                bail!("no such layer: '{}'", name);
            };
            if !seen.insert(name.clone()) {
                bail!("layer '{}' appears twice in the same stack", name);
            }
            if let (Some(since), Some(bound)) = (layer.since, layer.bound) {
                if since > bound {
                    bail!(
                        "layer '{}' has an inverted window ({}, {}]",
                        name,
                        since,
                        bound
                    );
                }
            }
            layers.push(LayerSpec {
                name,
                incarnation,
                window: Window {
                    since: layer.since,
                    bound: layer.bound,
                },
            });
        }
        let view = self.view_of(&layers);
        Ok(StackSpec { layers, view })
    }
}

impl<'s> Storage<'s> for LayeredStorage {
    type Tx = LayeredTx<'s>;

    fn storage_kind(&self) -> &'static str {
        "layered"
    }

    fn now_validity(&self) -> ValidityTs {
        // The sequence is assigned by storage at commit, so `'NOW'` is the pending stamp
        // rather than a clock reading, including for the ordinary entry points, which run
        // against a single-layer stack on the default layer.
        vld_at(PENDING_SEQ)
    }

    fn transact(&'s self, _write: bool) -> Result<Self::Tx> {
        let inner: &'s LayeredInner = &self.inner;
        let layers = bind_layers(inner, &self.stack)?;
        let catalog = inner
            .db
            .cf_handle(DEFAULT_LAYER)
            .ok_or_else(|| miette!("the default layer is missing"))?;
        Ok(LayeredTx {
            inner,
            tx: Some(inner.db.transaction()),
            stack: crate::storage::layered::tx::BoundStack::new(layers, self.stack.view.clone()),
            catalog,
            pending: vec![],
            relations: Default::default(),
        })
    }

    fn range_compact(&'s self, lower: &[u8], upper: &[u8]) -> Result<()> {
        for layer in self.stack.layers.iter() {
            if let Some(cf) = self.inner.db.cf_handle(&layer.name) {
                self.inner
                    .db
                    .compact_range_cf(&cf, Some(lower), Some(upper));
            }
        }
        Ok(())
    }

    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        let top = self
            .inner
            .db
            .cf_handle(&self.stack.layers[0].name)
            .ok_or_else(|| miette!("the top layer is missing"))?;
        let catalog = self
            .inner
            .db
            .cf_handle(DEFAULT_LAYER)
            .ok_or_else(|| miette!("the default layer is missing"))?;
        let mut batch = WriteBatchWithTransaction::<true>::default();
        let mut highest_stamp: Option<Seq> = None;
        for result in data {
            let (key, val) = result?;
            // Rows arrive pre-encoded here (this is the restore path) and keep the stamps
            // they were written with. That is right for reconstructing a store, and wrong the
            // moment the stamps outrun the instance's own counter, which is checked below.
            if let Some(vld) = crate::data::memcmp::tail_validity(&key) {
                let seq = vld.timestamp.0 .0;
                if seq != PENDING_SEQ {
                    highest_stamp = Some(highest_stamp.map_or(seq, |h: Seq| h.max(seq)));
                }
            }
            if is_catalog_key(&key) {
                batch.put_cf(&catalog, &key, &val);
            } else {
                batch.put_cf(&top, &key, &val);
            }
        }
        self.inner
            .db
            .write(batch)
            .into_diagnostic()
            .wrap_err("batch put failed")?;

        // Restoring rows stamped above the instance's own sequence would invert history: the
        // next commit would be stamped *below* rows that are already here, and a fork point
        // taken afterwards would not hold. Say so rather than let it happen quietly.
        if let Some(highest) = highest_stamp {
            let current = self.inner.db.latest_sequence_number() as Seq;
            if highest >= current {
                bail!(
                    "restored rows carry sequences up to {highest}, at or above this store's \
                     own sequence ({current}); writing to it would stamp new rows below \
                     restored ones. Rebuild the store by importing relations instead, which \
                     restamps."
                );
            }
        }
        Ok(())
    }
}

/// The validity a read at `seq` should use.
pub(crate) fn vld_at(seq: Seq) -> ValidityTs {
    ValidityTs(Reverse(seq))
}

impl Db<LayeredStorage> {
    /// Create a layer. This copies nothing: it is a metadata operation.
    pub fn create_layer(&self, id: impl Into<LayerId>) -> Result<()> {
        let id = id.into();
        let mut known = self.db.inner.layers.write().unwrap();
        if known.contains_key(&id.0) {
            bail!("layer '{}' already exists", id);
        }
        self.db
            .inner
            .db
            .create_cf(&id.0, &Options::default())
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to create layer '{}'", id))?;
        known.insert(
            id.0,
            self.db
                .inner
                .layer_incarnation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        Ok(())
    }

    /// Drop a layer and reclaim its storage. Stacks naming it will fail to build afterwards.
    pub fn drop_layer(&self, id: impl Into<LayerId>) -> Result<()> {
        let id = id.into();
        if id.0 == DEFAULT_LAYER {
            bail!("the default layer cannot be dropped: it carries the catalog");
        }
        let mut known = self.db.inner.layers.write().unwrap();
        if !known.contains_key(&id.0) {
            bail!("no such layer: '{}'", id);
        }
        self.db
            .inner
            .db
            .drop_cf(&id.0)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to drop layer '{}'", id))?;
        known.remove(&id.0);
        Ok(())
    }

    /// Every layer in the store, in name order.
    pub fn list_layers(&self) -> Result<Vec<LayerId>> {
        Ok(self
            .db
            .inner
            .layers
            .read()
            .unwrap()
            .iter()
            .map(|(n, _)| LayerId(n.clone()))
            .collect())
    }

    /// The last committed sequence. A fork point taken from here is stable: everything
    /// committed afterwards is stamped strictly above it.
    pub fn current_seq(&self) -> Result<Seq> {
        Ok(self.db.inner.db.latest_sequence_number() as Seq)
    }

    /// Run a script against a stack.
    ///
    /// `at` is a historical read bound composed with each layer's own bound; it is
    /// meaningless for a write, since a write's sequence is assigned at commit.
    pub fn run_on_stack(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
        stack: &Stack,
        at: Option<Seq>,
        mutability: ScriptMutability,
    ) -> Result<NamedRows> {
        if at.is_some() && mutability == ScriptMutability::Mutable {
            bail!("a historical read bound cannot be combined with a mutable script");
        }
        let spec = self.db.resolve(stack)?;
        let view = self.with_storage(self.db.with_stack(spec));
        let cur_vld = match at {
            Some(seq) => vld_at(seq),
            None => vld_at(PENDING_SEQ),
        };
        let ast = parse_script(
            script,
            &params,
            &view.get_fixed_rules(),
            crate::data::aggr::CustomAggrRegistries {
                meet: &view.get_custom_aggrs(),
                bounded: &view.get_custom_bounded_meets(),
            },
            cur_vld,
        )?;
        view.run_script_ast(ast, cur_vld, mutability)
    }
}
