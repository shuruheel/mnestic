/*
 * Layered storage: what the global catalog says about a relation.
 *
 * Stackability is a property of a relation's schema, fixed when it is created: a relation whose
 * last key column is a validity can express a cross-layer retraction, and one without cannot.
 * Nothing here decides policy; it reports what the catalog holds so that the stack gates in
 * `tx.rs` and the skip rules in `flatten.rs` can decide from the relation alone, before any row
 * is touched.
 */

use std::sync::Arc;

use miette::{IntoDiagnostic, Result, WrapErr};
use smartstring::{LazyCompact, SmartString};
use rocksdb::BoundColumnFamily;

use crate::data::relation::ColType;
use crate::data::tuple::TupleT;
use crate::data::value::{DataValue, LARGEST_UTF_CHAR};
use crate::runtime::relation::{RelationHandle, RelationId};
use crate::storage::layered::iter::LayeredTxn;

/// One relation, as the global catalog describes it.
#[derive(Clone, Debug)]
pub(crate) struct RelInfo {
    pub(crate) id: RelationId,
    pub(crate) name: SmartString<LazyCompact>,
    /// Whether the relation carries a validity, and so can express a cross-layer retraction.
    pub(crate) stackable: bool,
    /// Index relations store their structure as ordinary rows. They compose through a stack but
    /// are never flattened; the destination's are dropped and rebuilt.
    pub(crate) is_index: bool,
    /// How many columns the key holds, for decoding a stored key back into values.
    pub(crate) n_keys: usize,
}

/// Every relation in the store, ordered by relation id.
///
/// Ids are big-endian at the head of every key, so id order is keyspace order: iterating
/// visits relations in the order their rows appear on disk, which is what lets a paged scan
/// resume from a single key. Held contiguously and searched rather than walked as a tree,
/// since a lookup happens on every operation a stack gates.
pub(crate) struct Catalog {
    relations: Vec<RelInfo>,
}

impl Catalog {
    /// What the catalog says about one relation, or `None` if it holds no such id.
    pub(crate) fn get(&self, id: u64) -> Option<&RelInfo> {
        self.relations
            .binary_search_by_key(&id, |rel| rel.id.0)
            .ok()
            .map(|at| &self.relations[at])
    }

    /// Every relation, in id order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &RelInfo> {
        self.relations.iter()
    }
}

/// Read the catalog.
///
/// The catalog is global (one copy, in the default layer), so this is the same answer through
/// every stack.
pub(crate) fn catalog_relations(
    txn: &LayeredTxn<'_>,
    catalog: &Arc<BoundColumnFamily<'_>>,
) -> Result<Catalog> {
    let lower = vec![DataValue::from("")].encode_as_key(RelationId::SYSTEM);
    let upper =
        vec![DataValue::from(String::from(LARGEST_UTF_CHAR))].encode_as_key(RelationId::SYSTEM);
    let mut relations = vec![];
    let mut it = txn.raw_iterator_cf(catalog);
    it.seek(&lower);
    while let Some(k) = it.key() {
        if k >= upper.as_slice() {
            break;
        }
        let meta = RelationHandle::decode(it.value().unwrap_or_default())?;
        let stackable = matches!(
            meta.metadata.keys.last().map(|c| &c.typing.coltype),
            Some(ColType::Validity)
        );
        relations.push(RelInfo {
            id: meta.id,
            // Cozo names an index after its parent, `relation:index`; that colon is the
            // catalog's own marker, and what `::relations` reports on.
            is_index: meta.name.contains(':'),
            name: meta.name.clone(),
            stackable,
            n_keys: meta.metadata.keys.len(),
        });
        it.next();
    }
    it.status()
        .into_diagnostic()
        .wrap_err("failed to read the catalog")?;
    // The catalog is stored by name, so ordering by id is this function's job.
    relations.sort_unstable_by_key(|rel| rel.id.0);
    Ok(Catalog { relations })
}

/// The relation a key belongs to, read from its prefix.
#[inline]
pub(crate) fn relation_of(key: &[u8]) -> Option<u64> {
    if key.len() < 8 {
        return None;
    }
    Some(u64::from_be_bytes([
        key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7],
    ]))
}
