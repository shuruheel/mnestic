/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Read-only Cypher surface (mnestic fork; behind the `cypher` feature).
//!
//! Translates a subset of openCypher into CozoScript so the engine can be
//! evaluated and adopted without first learning Datalog. Datalog stays the
//! native, full-power language; this is a read-only on-ramp (no write clauses).
//! Design, scope, and the settled decisions are in `docs/specs/cypher-read.md`.
//!
//! Status: **shipped (0.9.0, alpha)** behind the off-by-default `cypher`
//! feature — parser, translator, and the `DbInstance::run_cypher` /
//! `cypher_to_script` entry points (in `src/lib.rs`) are all live. See the
//! spec's §10 step log; flip-default-on criteria are in §10 step 6.

mod ast;
mod parse;
mod schema;
mod translate;

pub use schema::{CypherGraphSchema, EdgeMap, NodeMap};
pub(crate) use translate::CypherScript;

/// Parse + translate a read-only Cypher query into runnable CozoScript against a
/// property-graph schema. The entry point for `DbInstance::run_cypher`.
pub(crate) fn build_cypher_script(
    query: &str,
    schema: &CypherGraphSchema,
) -> miette::Result<CypherScript> {
    translate::cypher_to_script(&parse::parse_cypher(query)?, schema)
}
