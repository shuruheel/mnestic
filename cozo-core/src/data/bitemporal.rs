/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * Portions Copyright 2022, The Cozo Project Authors (the seek-loop shape
 * follows `check_key_for_validity` in data/tuple.rs).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Two-level bitemporal resolution (mnestic fork; bitemporality step 4b of
//! `docs/specs/bitemporality.md` §3/§4).
//!
//! Key layout of a bitemporal relation: `[rel][k…][vt: Validity][tt: Validity]`,
//! both temporal components newest-first. A **vt-group** is all rows of a key
//! sharing the vt *timestamp* — which physically spans TWO contiguous runs
//! (the assert run, then the retract run), because the vt flag byte sorts
//! between the vt timestamp and tt. Resolution therefore probes **both runs**
//! for the greatest `tt ≤ T` — examining only the assert run would let a
//! later-recorded cessation be silently shadowed (spec §3).
//!
//! Modes:
//! - **resolve-key** (a vt point `V` is given): walk vt-groups ≤ V newest
//!   first; the first group holding any record at `tt ≤ T` decides the key —
//!   assertion → emit (≤ 1 row per key), retraction → believed-deleted, emit
//!   nothing; group empty at T → fall through to the next older group. Equal
//!   `(vt, tt)` ties resolve to the assertion.
//! - **resolve-groups** (no vt point): emit, for EVERY vt-group, its
//!   `tt ≤ T` belief — retraction records included as rows, mirroring what a
//!   vt-only relation's bare scan shows (the §4 migration invariant).
//!
//! The driver is probe-based: a backend only answers "first key–value at or
//! after this bound" — the same contract the single-axis skip iterators use —
//! so one generic implementation serves every backend. Hot backends can
//! override `range_bitemporal_scan_tuple` with a pinned-iterator seek loop
//! later (step 6 measures). All timestamp comparisons below are on RAW i64
//! microseconds (`.0 .0`) — never on the `Reverse`-wrapped ordering — to keep
//! the newest-first encoding from inverting a comparison silently.

use std::cmp::Reverse;

use miette::Result;

use crate::data::tuple::{decode_tuple_from_key, Tuple, TupleT, DEFAULT_SIZE_HINT};
use crate::data::value::{DataValue, Validity, ValidityTs};
use crate::runtime::relation::extend_tuple_from_v;
use crate::runtime::relation::RelationId;

/// Sorts after every real row at its position (MIN timestamp, retract flag).
const AXIS_TERMINAL: Validity = Validity {
    timestamp: ValidityTs(Reverse(i64::MIN)),
    is_assert: Reverse(false),
};

fn axis(ts: ValidityTs, is_assert: bool) -> Validity {
    Validity {
        timestamp: ts,
        is_assert: Reverse(is_assert),
    }
}

struct Landing {
    tuple: Tuple,
    v_bytes: Vec<u8>,
}

impl Landing {
    fn plain_key(&self) -> &[DataValue] {
        &self.tuple[..self.tuple.len() - 2]
    }
    fn vt_raw(&self) -> i64 {
        match &self.tuple[self.tuple.len() - 2] {
            DataValue::Validity(v) => v.timestamp.0 .0,
            _ => unreachable!("bitemporal key without vt component"),
        }
    }
    fn vt_is_assert(&self) -> bool {
        match &self.tuple[self.tuple.len() - 2] {
            DataValue::Validity(v) => v.is_assert.0,
            _ => unreachable!(),
        }
    }
    fn tt_raw(&self) -> i64 {
        match &self.tuple[self.tuple.len() - 1] {
            DataValue::Validity(v) => v.timestamp.0 .0,
            _ => unreachable!("bitemporal key without tt component"),
        }
    }
    fn into_row(self) -> Tuple {
        let mut out = self.tuple;
        extend_tuple_from_v(&mut out, &self.v_bytes);
        out
    }
}

enum Phase {
    /// Probing for an assert-run record at `tt ≤ T`. When `group` is `None`
    /// the bound is only positional (fresh key / next group): the landing
    /// names the (key, group) to resolve and is re-probed at T.
    ProbeAssert {
        key: Option<Vec<DataValue>>,
        group: Option<i64>,
    },
    /// Assert candidate (if any) held; probing the retract run at T.
    ProbeRetract {
        key: Vec<DataValue>,
        group: i64,
        cand: Option<Landing>,
    },
    Done,
}

/// The generic two-level scan over one bound range (whole relation or a
/// prefix). `probe(bound)` = first `(key, value)` with `key ≥ bound` inside
/// the range, or `None`.
pub(crate) struct BitemporalIter<F> {
    probe: F,
    rel_id: RelationId,
    /// `Some(raw µs)` = resolve-key mode at that vt point; `None` = groups.
    vt_at_raw: Option<i64>,
    tt_at_raw: i64,
    bound: Vec<u8>,
    phase: Phase,
}

impl<F> BitemporalIter<F>
where
    F: FnMut(&[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>>,
{
    pub(crate) fn new(
        probe: F,
        lower: Vec<u8>,
        vt_at: Option<ValidityTs>,
        tt_at: ValidityTs,
    ) -> Self {
        let rel_id = RelationId::raw_decode(&lower);
        Self {
            probe,
            rel_id,
            vt_at_raw: vt_at.map(|v| v.0 .0),
            tt_at_raw: tt_at.0 .0,
            bound: lower,
            phase: Phase::ProbeAssert {
                key: None,
                group: None,
            },
        }
    }

    fn encode(&self, plain: &[DataValue], vt: Validity, tt: Validity) -> Vec<u8> {
        let mut t: Tuple = plain.to_vec();
        t.push(DataValue::Validity(vt));
        t.push(DataValue::Validity(tt));
        t.encode_as_key(self.rel_id)
    }

    fn ts(raw: i64) -> ValidityTs {
        ValidityTs(Reverse(raw))
    }

    /// After a group resolves (or a key dies): resolve-key moves to the next
    /// key; resolve-groups moves to the key's next older group.
    fn advance(&mut self, plain: &[DataValue], group: i64) {
        match self.vt_at_raw {
            Some(_) => {
                self.bound = self.encode(plain, AXIS_TERMINAL, AXIS_TERMINAL);
                self.phase = Phase::ProbeAssert {
                    key: None,
                    group: None,
                };
            }
            None => {
                // past the entire retract run of `group`
                self.bound = self.encode(plain, axis(Self::ts(group), false), AXIS_TERMINAL);
                self.phase = Phase::ProbeAssert {
                    key: Some(plain.to_vec()),
                    group: None,
                };
            }
        }
    }

    /// Direct the walk at (key, group)'s assert run, T-bounded.
    fn aim_assert(&mut self, plain: Vec<DataValue>, group: i64) {
        self.bound = self.encode(
            &plain,
            axis(Self::ts(group), true),
            axis(Self::ts(self.tt_at_raw), true),
        );
        self.phase = Phase::ProbeAssert {
            key: Some(plain),
            group: Some(group),
        };
    }

    /// Direct the walk at (key, group)'s retract run, T-bounded.
    fn aim_retract(&mut self, plain: Vec<DataValue>, group: i64, cand: Option<Landing>) {
        self.bound = self.encode(
            &plain,
            axis(Self::ts(group), false),
            axis(Self::ts(self.tt_at_raw), true),
        );
        self.phase = Phase::ProbeRetract {
            key: plain,
            group,
            cand,
        };
    }

    /// Fresh landing (new key, or next group after a resolution): pick the
    /// (key, group) to resolve and re-probe its assert run at T. In
    /// resolve-key mode a fresh KEY starts at `min-newer-bound V`; a fresh
    /// group of the SAME key (resolve-groups) starts at the landing's group.
    fn classify_fresh(&mut self, landing: &Landing, same_key_walk: bool) {
        let plain = landing.plain_key().to_vec();
        let group = match self.vt_at_raw {
            Some(v_raw) if !same_key_walk => {
                if landing.vt_raw() > v_raw {
                    // the key's newest group is newer than V: start at V
                    v_raw
                } else {
                    landing.vt_raw()
                }
            }
            _ => landing.vt_raw(),
        };
        self.aim_assert(plain, group);
    }

    fn next_inner(&mut self) -> Result<Option<Tuple>> {
        loop {
            if matches!(self.phase, Phase::Done) {
                return Ok(None);
            }
            let landing = match (self.probe)(&self.bound)? {
                None => {
                    // Range exhausted: a held assert candidate still wins.
                    let prev = std::mem::replace(&mut self.phase, Phase::Done);
                    if let Phase::ProbeRetract { cand: Some(c), .. } = prev {
                        return Ok(Some(c.into_row()));
                    }
                    return Ok(None);
                }
                Some((k, v)) => Landing {
                    tuple: decode_tuple_from_key(&k, DEFAULT_SIZE_HINT),
                    v_bytes: v,
                },
            };

            let phase = std::mem::replace(&mut self.phase, Phase::Done);
            match phase {
                Phase::Done => unreachable!(),
                Phase::ProbeAssert { key, group } => {
                    let same_key = key.as_deref() == Some(landing.plain_key());
                    match (same_key, group) {
                        // Intent-directed probe of (key, group)'s assert run:
                        (true, Some(g)) => {
                            let plain = key.expect("same_key");
                            if landing.vt_raw() == g && landing.vt_is_assert() {
                                // greatest assert tt ≤ T of the group (the
                                // T-bound guarantees landing.tt ≤ T here)
                                self.aim_retract(plain, g, Some(landing));
                            } else if landing.vt_raw() == g {
                                // inside the retract run
                                if landing.tt_raw() <= self.tt_at_raw {
                                    // the group's belief is this retraction
                                    self.advance(&plain, g);
                                    if self.vt_at_raw.is_none() {
                                        return Ok(Some(landing.into_row()));
                                    }
                                } else {
                                    // newest retract is beyond T: probe at T
                                    self.aim_retract(plain, g, None);
                                }
                            } else {
                                // past the group entirely: no assert ≤ T; the
                                // retract run may still hold a record ≤ T
                                self.aim_retract(plain, g, None);
                            }
                        }
                        // Positional landing: fresh key or next group.
                        (true, None) => self.classify_fresh(&landing, true),
                        (false, _) => self.classify_fresh(&landing, false),
                    }
                }
                Phase::ProbeRetract { key, group, cand } => {
                    let in_run = landing.plain_key() == key.as_slice()
                        && landing.vt_raw() == group
                        && !landing.vt_is_assert()
                        && landing.tt_raw() <= self.tt_at_raw;
                    if in_run {
                        let assert_wins = match &cand {
                            // equal (vt, tt) ties resolve to the assertion
                            Some(c) => landing.tt_raw() <= c.tt_raw(),
                            None => false,
                        };
                        if assert_wins {
                            let c = cand.expect("assert_wins");
                            self.advance(&key, group);
                            return Ok(Some(c.into_row()));
                        }
                        // retraction wins
                        self.advance(&key, group);
                        if self.vt_at_raw.is_none() {
                            return Ok(Some(landing.into_row()));
                        }
                    } else {
                        // no retract ≤ T in the group
                        match cand {
                            Some(c) => {
                                self.advance(&key, group);
                                return Ok(Some(c.into_row()));
                            }
                            None => {
                                // group empty at T → fall through to whatever
                                // the landing names (older group or next key)
                                let same_key = landing.plain_key() == key.as_slice();
                                self.classify_fresh(&landing, same_key);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl<F> Iterator for BitemporalIter<F>
where
    F: FnMut(&[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>>,
{
    type Item = Result<Tuple>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.next_inner() {
            Ok(Some(t)) => Some(Ok(t)),
            Ok(None) => None,
            Err(e) => {
                self.phase = Phase::Done;
                Some(Err(e))
            }
        }
    }
}
