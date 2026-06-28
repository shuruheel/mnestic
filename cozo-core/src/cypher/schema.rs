/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The caller-supplied property-graph schema that maps Cypher's labeled-node /
//! typed-relationship model onto stored relations. mnestic stores arbitrary
//! relations and has no built-in "node"/"edge" notion, so the mapping is data,
//! not policy. Supports both modeling conventions (see `docs/specs/cypher-read.md`
//! §3): relation-per-label/type (`label_col`/`type_col` = `None`) and a shared
//! relation with a discriminator column (e.g. MindGraph's `node{node_type}` /
//! reified `edge{edge_type}`).

use crate::data::value::DataValue;

/// How Cypher's property-graph model maps onto stored relations. Supplied per
/// call (a persisted/named view is a later convenience).
#[derive(Debug, Clone, Default)]
pub struct CypherGraphSchema {
    /// One entry per Cypher node label exposed.
    pub nodes: Vec<NodeMap>,
    /// One entry per Cypher relationship type exposed.
    pub edges: Vec<EdgeMap>,
}

/// Maps a Cypher node label onto a stored relation.
#[derive(Debug, Clone)]
pub struct NodeMap {
    /// The Cypher label, e.g. `"Person"`.
    pub label: String,
    /// The stored relation holding these nodes, e.g. `"person"` or `"node"`.
    pub relation: String,
    /// The column holding node identity, e.g. `"id"` / `"uid"`.
    pub id_col: String,
    /// Discriminator column when `relation` is shared across labels
    /// (e.g. `"node_type"`); `None` means the relation *is* the label.
    pub label_col: Option<String>,
    /// Value to match in `label_col`; defaults to `label` (as a string) when
    /// `label_col` is set and this is `None`.
    pub label_value: Option<DataValue>,
    /// Optional CozoScript predicate always ANDed into accesses of this relation
    /// (e.g. a soft-delete guard like `tombstone_at == 0.0`). A general
    /// mechanism — the engine does not interpret it.
    pub filter: Option<String>,
}

/// Maps a Cypher relationship type onto a stored relation.
#[derive(Debug, Clone)]
pub struct EdgeMap {
    /// The Cypher relationship type, e.g. `"KNOWS"`.
    pub rel_type: String,
    /// The stored relation holding these edges, e.g. `"knows"` or `"edge"`.
    pub relation: String,
    /// Column holding the source node id.
    pub from_col: String,
    /// Column holding the destination node id.
    pub to_col: String,
    /// Discriminator column when `relation` is shared across types
    /// (e.g. `"edge_type"`); `None` means the relation *is* the type.
    pub type_col: Option<String>,
    /// Value to match in `type_col`; defaults to `rel_type` (as a string) when
    /// `type_col` is set and this is `None`.
    pub type_value: Option<DataValue>,
    /// Explicit edge-identity column (e.g. a reified edge `"uid"`). Used for
    /// relationship-uniqueness (edge-isomorphism); when `None`, identity is the
    /// `(from_col, to_col)` tuple plus `type_col` if present. Relations that
    /// permit parallel edges must set this.
    pub eid_col: Option<String>,
    /// Optional CozoScript predicate always ANDed into accesses of this relation.
    pub filter: Option<String>,
}

impl CypherGraphSchema {
    /// Look up the node mapping for a Cypher label.
    pub(crate) fn node(&self, label: &str) -> Option<&NodeMap> {
        self.nodes.iter().find(|n| n.label == label)
    }
    /// Look up the edge mapping for a Cypher relationship type.
    pub(crate) fn edge(&self, rel_type: &str) -> Option<&EdgeMap> {
        self.edges.iter().find(|e| e.rel_type == rel_type)
    }
}
