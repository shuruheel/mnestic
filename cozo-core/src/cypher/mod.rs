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
//! Status: **step 2 — schema types + grammar + parser**. The translator
//! (Cypher AST -> CozoScript string) and the `run_cypher` entry point land in
//! later steps; the parser is exercised by its own unit tests for now.

mod ast;
mod parse;
mod schema;

pub use schema::{CypherGraphSchema, EdgeMap, NodeMap};
