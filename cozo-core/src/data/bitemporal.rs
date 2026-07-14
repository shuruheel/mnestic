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
//! `docs/specs/bitemporality.md` §3/§4; step-6 performance shape).
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
//! so one generic implementation serves every backend. Step-6 performance
//! shape (the generic per-probe `range_scan` default measured 4–8× the
//! vt-only baseline on scans):
//! - the sqlite and rocksdb backends override `range_bitemporal_scan_tuple`
//!   with a pinned cursor driven through [`HybridProbe`] (sequential `step`
//!   speculation before a real seek);
//! - probe bounds are spliced at the byte level — a `Validity` key component
//!   is exactly [`VLD_LEN`] bytes — instead of re-encoding tuples;
//! - landings decode only the two temporal axes; the full tuple is decoded
//!   only for EMITTED rows;
//! - a landing that already answers the next (monotone) bound is reused
//!   without touching the backend at all.
//!
//! All timestamp comparisons below are on RAW i64 microseconds — never on
//! the `Reverse`-wrapped ordering — to keep the newest-first encoding from
//! inverting a comparison silently.

use miette::{bail, Result};

use crate::data::memcmp::{order_decode_i64, order_encode_i64, VLD_TAG};
use crate::data::tuple::{decode_tuple_from_key, Tuple, DEFAULT_SIZE_HINT};
use crate::data::value::ValidityTs;
use crate::runtime::relation::try_extend_tuple_from_v;

/// A pinned, forward-only cursor over one bound range — the backend surface
/// the step-6 seek overrides plug into `HybridProbe`.
pub(crate) trait SeekCursor {
    /// Position at the first row with `key ≥ bound` inside the range.
    fn seek(&mut self, bound: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>>;
    /// Advance one row sequentially; `None` when the range is exhausted.
    fn step(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>>;
}

/// Probe implementation over a pinned cursor (bitemporality step 6). The
/// generic default opens a fresh range per probe — correct but dominated by
/// per-probe setup (statement prepare / iterator construction). This one
/// keeps a single cursor and answers each probe as cheaply as the data
/// allows:
///
/// 1. `bound ≤ last-returned key` → the cached row IS the answer: the walk's
///    probe bounds never regress below the bound that produced the cache, so
///    `[bound, last)` was already proven empty.
/// 2. otherwise try ONE sequential `step()` — in shallow histories (few
///    corrections per run) the target is almost always the physically next
///    row, and a sequential step is far cheaper than a seek.
/// 3. undershot → real `seek(bound)`.
pub(crate) struct HybridProbe<C> {
    cursor: C,
    last: Option<(Vec<u8>, Vec<u8>)>,
    primed: bool,
    #[cfg(debug_assertions)]
    prev_bound: Vec<u8>,
}

impl<C: SeekCursor> HybridProbe<C> {
    pub(crate) fn new(cursor: C) -> Self {
        Self {
            cursor,
            last: None,
            primed: false,
            #[cfg(debug_assertions)]
            prev_bound: vec![],
        }
    }

    /// `far` = the caller knows the target is a positional skip (past a
    /// whole key or group), never the physically next row — skip the
    /// speculative step and seek directly.
    pub(crate) fn probe(&mut self, bound: &[u8], far: bool) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        #[cfg(debug_assertions)]
        {
            // the cache-hit shortcut in (1) is only sound on monotone bounds
            debug_assert!(
                bound >= self.prev_bound.as_slice(),
                "bitemporal probe bounds must never regress"
            );
            self.prev_bound = bound.to_vec();
        }
        if self.primed && !far {
            if let Some((lk, lv)) = &self.last {
                if bound <= lk.as_slice() {
                    return Ok(Some((lk.clone(), lv.clone())));
                }
                match self.cursor.step()? {
                    None => {
                        // range exhausted past the cached row ⇒ nothing ≥ bound
                        self.last = None;
                        return Ok(None);
                    }
                    Some((k, v)) => {
                        if k.as_slice() >= bound {
                            self.last = Some((k.clone(), v.clone()));
                            return Ok(Some((k, v)));
                        }
                        // undershot: fall through to the real seek
                    }
                }
            }
        }
        self.primed = true;
        match self.cursor.seek(bound)? {
            None => {
                self.last = None;
                Ok(None)
            }
            Some((k, v)) => {
                self.last = Some((k.clone(), v.clone()));
                Ok(Some((k, v)))
            }
        }
    }
}

/// Encoded width of one `Validity` key component: tag + flipped BE i64 + flag.
const VLD_LEN: usize = 10;
/// The two trailing temporal axes of a bitemporal key.
const AXES_LEN: usize = 2 * VLD_LEN;

/// Append the memcmp encoding of a `Validity` (mirrors
/// `memcmp::encode_datavalue`'s `Validity` arm byte for byte).
fn push_vld(buf: &mut Vec<u8>, ts_raw: i64, is_assert: bool) {
    buf.push(VLD_TAG);
    buf.extend_from_slice(&(!order_encode_i64(ts_raw)).to_be_bytes());
    buf.push(!is_assert as u8);
}

/// Decode one `Validity` key component → (raw µs, is_assert).
fn read_vld(bytes: &[u8]) -> Result<(i64, bool)> {
    if bytes.len() != VLD_LEN || bytes[0] != VLD_TAG {
        bail!("corrupt temporal component in a bitemporal key");
    }
    let ts_flipped = u64::from_be_bytes(bytes[1..9].try_into().unwrap());
    Ok((order_decode_i64(!ts_flipped), bytes[9] == 0))
}

/// Sorts after every real row at its position (MIN timestamp, retract flag).
const TERMINAL: (i64, bool) = (i64::MIN, false);

struct Landing {
    raw_key: Vec<u8>,
    v_bytes: Vec<u8>,
    plain_len: usize,
    vt_raw: i64,
    vt_assert: bool,
    tt_raw: i64,
}

impl Landing {
    fn parse(raw_key: Vec<u8>, v_bytes: Vec<u8>) -> Result<Self> {
        let n = raw_key.len();
        if n < AXES_LEN {
            bail!("corrupt bitemporal key: shorter than its two temporal axes");
        }
        let (vt_raw, vt_assert) = read_vld(&raw_key[n - AXES_LEN..n - VLD_LEN])?;
        let (tt_raw, tt_assert) = read_vld(&raw_key[n - VLD_LEN..])?;
        // On bitemporal relations the tt axis is engine-stamped: its ts is
        // never MIN and its flag byte is reserved-0. A corrupt row carrying
        // either sorts AT the `advance` TERMINAL bound and would spin the
        // walk forever — bail loudly instead.
        if tt_raw == i64::MIN || !tt_assert {
            bail!("corrupt bitemporal key: reserved transaction-time value");
        }
        Ok(Self {
            plain_len: n - AXES_LEN,
            raw_key,
            v_bytes,
            vt_raw,
            vt_assert,
            tt_raw,
        })
    }

    fn plain(&self) -> &[u8] {
        &self.raw_key[..self.plain_len]
    }

    fn into_row(self) -> Result<Tuple> {
        let mut out = decode_tuple_from_key(&self.raw_key, DEFAULT_SIZE_HINT);
        try_extend_tuple_from_v(&mut out, &self.v_bytes)?;
        Ok(out)
    }
}

enum Phase {
    /// Probing for an assert-run record at `tt ≤ T`. When `group` is `None`
    /// the bound is only positional (fresh key / next group): the landing
    /// names the (key, group) to resolve and is re-probed at T. `key` holds
    /// the encoded plain-key prefix bytes.
    ProbeAssert {
        key: Option<Vec<u8>>,
        group: Option<i64>,
    },
    /// Assert candidate (if any) held; probing the retract run at T.
    ProbeRetract {
        key: Vec<u8>,
        group: i64,
        cand: Option<Landing>,
    },
    Done,
}

/// The generic two-level scan over one bound range (whole relation or a
/// prefix). `probe(bound)` = first `(key, value)` with `key ≥ bound` inside
/// the range, or `None`.
pub(crate) struct BitemporalIter<F> {
    /// `probe(bound, far)` = first row ≥ bound; `far` hints that the target
    /// is a positional skip, never the physically next row.
    probe: F,
    /// `Some(raw µs)` = resolve-key mode at that vt point; `None` = groups.
    vt_at_raw: Option<i64>,
    tt_at_raw: i64,
    bound: Vec<u8>,
    /// The pending bound is a positional skip (set by `advance`): the target
    /// is never the physically next row, so a hybrid probe should seek.
    bound_is_far: bool,
    phase: Phase,
    /// The last landing, kept across a transition whose new bound it is
    /// known to still answer (probe bounds are monotone, so `bound ≤
    /// landing` proves `[bound, landing)` empty) — saves the backend probe.
    pending: Option<Landing>,
}

impl<F> BitemporalIter<F>
where
    F: FnMut(&[u8], bool) -> Result<Option<(Vec<u8>, Vec<u8>)>>,
{
    pub(crate) fn new(
        probe: F,
        lower: Vec<u8>,
        vt_at: Option<ValidityTs>,
        tt_at: ValidityTs,
    ) -> Self {
        Self {
            probe,
            vt_at_raw: vt_at.map(|v| v.0 .0),
            tt_at_raw: tt_at.0 .0,
            bound: lower,
            bound_is_far: true,
            phase: Phase::ProbeAssert {
                key: None,
                group: None,
            },
            pending: None,
        }
    }

    fn set_bound(&mut self, plain: &[u8], vt: (i64, bool), tt: (i64, bool)) {
        self.bound.clear();
        self.bound.extend_from_slice(plain);
        push_vld(&mut self.bound, vt.0, vt.1);
        push_vld(&mut self.bound, tt.0, tt.1);
        self.bound_is_far = false;
    }

    /// After a group resolves (or a key dies): resolve-key moves to the next
    /// key; resolve-groups moves to the key's next older group.
    fn advance(&mut self, plain: &[u8], group: i64) {
        match self.vt_at_raw {
            Some(_) => {
                self.set_bound(plain, TERMINAL, TERMINAL);
                self.bound_is_far = true;
                self.phase = Phase::ProbeAssert {
                    key: None,
                    group: None,
                };
            }
            None => {
                // past the entire retract run of `group`
                self.set_bound(plain, (group, false), TERMINAL);
                self.bound_is_far = true;
                self.phase = Phase::ProbeAssert {
                    key: Some(plain.to_vec()),
                    group: None,
                };
            }
        }
    }

    /// Direct the walk at (key, group)'s assert run, T-bounded.
    fn aim_assert(&mut self, plain: Vec<u8>, group: i64) {
        self.set_bound(&plain, (group, true), (self.tt_at_raw, true));
        self.phase = Phase::ProbeAssert {
            key: Some(plain),
            group: Some(group),
        };
    }

    /// Direct the walk at (key, group)'s retract run, T-bounded.
    fn aim_retract(&mut self, plain: Vec<u8>, group: i64, cand: Option<Landing>) {
        self.set_bound(&plain, (group, false), (self.tt_at_raw, true));
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
        let group = match self.vt_at_raw {
            Some(v_raw) if !same_key_walk => {
                if landing.vt_raw > v_raw {
                    // the key's newest group is newer than V: start at V
                    v_raw
                } else {
                    landing.vt_raw
                }
            }
            _ => landing.vt_raw,
        };
        self.aim_assert(landing.plain().to_vec(), group);
    }

    fn next_inner(&mut self) -> Result<Option<Tuple>> {
        loop {
            if matches!(self.phase, Phase::Done) {
                return Ok(None);
            }
            // Reuse the pending landing when it still answers the (monotone)
            // bound; otherwise it is stale — drop it and hit the backend.
            let landing = match self.pending.take() {
                Some(l) if self.bound.as_slice() <= l.raw_key.as_slice() => l,
                _ => match (self.probe)(&self.bound, self.bound_is_far)? {
                    None => {
                        // Range exhausted: a held assert candidate still wins.
                        let prev = std::mem::replace(&mut self.phase, Phase::Done);
                        if let Phase::ProbeRetract { cand: Some(c), .. } = prev {
                            return c.into_row().map(Some);
                        }
                        return Ok(None);
                    }
                    Some((k, v)) => Landing::parse(k, v)?,
                },
            };

            let phase = std::mem::replace(&mut self.phase, Phase::Done);
            match phase {
                Phase::Done => unreachable!(),
                Phase::ProbeAssert { key, group } => {
                    let same_key = key.as_deref() == Some(landing.plain());
                    match (same_key, group) {
                        // Intent-directed probe of (key, group)'s assert run:
                        (true, Some(g)) => {
                            let plain = key.expect("same_key");
                            if landing.vt_raw == g && landing.vt_assert {
                                // greatest assert tt ≤ T of the group (the
                                // T-bound — probed or reused — guarantees
                                // landing.tt ≤ T here)
                                self.aim_retract(plain, g, Some(landing));
                            } else if landing.vt_raw == g {
                                // inside the retract run
                                if landing.tt_raw <= self.tt_at_raw {
                                    // the group's belief is this retraction
                                    self.advance(&plain, g);
                                    if self.vt_at_raw.is_none() {
                                        return landing.into_row().map(Some);
                                    }
                                } else {
                                    // newest retract is beyond T: probe at T
                                    self.aim_retract(plain, g, None);
                                }
                            } else {
                                // past the group entirely: no assert ≤ T; the
                                // retract run may still hold a record ≤ T
                                self.aim_retract(plain, g, None);
                                self.pending = Some(landing);
                            }
                        }
                        // Positional landing: fresh key or next group.
                        (true, None) => {
                            self.classify_fresh(&landing, true);
                            self.pending = Some(landing);
                        }
                        (false, _) => {
                            self.classify_fresh(&landing, false);
                            self.pending = Some(landing);
                        }
                    }
                }
                Phase::ProbeRetract { key, group, cand } => {
                    let in_run = landing.plain() == key.as_slice()
                        && landing.vt_raw == group
                        && !landing.vt_assert
                        && landing.tt_raw <= self.tt_at_raw;
                    if in_run {
                        let assert_wins = match &cand {
                            // equal (vt, tt) ties resolve to the assertion
                            Some(c) => landing.tt_raw <= c.tt_raw,
                            None => false,
                        };
                        if assert_wins {
                            let c = cand.expect("assert_wins");
                            self.advance(&key, group);
                            return c.into_row().map(Some);
                        }
                        // retraction wins
                        self.advance(&key, group);
                        if self.vt_at_raw.is_none() {
                            return landing.into_row().map(Some);
                        }
                    } else {
                        // no retract ≤ T in the group
                        match cand {
                            Some(c) => {
                                self.advance(&key, group);
                                // the landing (often the next key's first
                                // row) usually answers the advance bound
                                self.pending = Some(landing);
                                return c.into_row().map(Some);
                            }
                            None => {
                                // group empty at T → fall through to whatever
                                // the landing names (older group or next key)
                                let same_key = landing.plain() == key.as_slice();
                                self.classify_fresh(&landing, same_key);
                                self.pending = Some(landing);
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
    F: FnMut(&[u8], bool) -> Result<Option<(Vec<u8>, Vec<u8>)>>,
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
