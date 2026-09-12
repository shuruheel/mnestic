/*
 * A differential oracle, and the algebraic properties it makes testable.
 *
 * The model below is a deliberately naive restatement of the stack-resolution and flatten rules
 * over ordered maps: no iterators, no encoding, no seeks, no windows expressed as byte ranges.
 * That is the point. It shares the *rules* with the engine but none of the machinery, so what
 * it catches is machinery: a seek that lands one row early, a window applied to the wrong
 * layer, a merge that drops the last key of a range.
 *
 * It cannot catch a misreading of the rules that both implementations share. The explicit
 * cases in the sibling modules are what stand behind that.
 */

use std::collections::BTreeMap;

use miette::Result;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::{base, Fixture};
use crate::storage::layered::{LayerRef, Seq, Stack};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Row {
    seq: Seq,
    assertive: bool,
    val: String,
}

/// A window, as the model sees it: an exclusive floor and an inclusive ceiling.
#[derive(Copy, Clone, Debug, Default)]
struct Win {
    since: Option<Seq>,
    bound: Option<Seq>,
}

impl Win {
    fn admits(&self, seq: Seq, at: Option<Seq>) -> bool {
        if let Some(since) = self.since {
            if seq <= since {
                return false;
            }
        }
        // Two independent ceilings, composed by `min`: the layer's and the query's.
        let ceiling = match (self.bound, at) {
            (Some(b), Some(a)) => Some(b.min(a)),
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        match ceiling {
            Some(c) => seq <= c,
            None => true,
        }
    }
}

type View = Vec<(String, Win)>;

#[derive(Default)]
struct Model {
    /// layer -> key -> versions, in no particular order.
    layers: BTreeMap<String, BTreeMap<String, Vec<Row>>>,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct ModelStats {
    copied: u64,
    deduped: u64,
    dropped: u64,
}

impl Model {
    fn layer(&mut self, name: &str) -> &mut BTreeMap<String, Vec<Row>> {
        self.layers.entry(name.to_string()).or_default()
    }

    fn write(&mut self, layer: &str, id: &str, row: Row) {
        self.layer(layer).entry(id.to_string()).or_default().push(row);
    }

    /// The winning version of every key through a view: newest by sequence, and on a tie the
    /// one from the higher layer.
    fn resolve(&self, view: &View, at: Option<Seq>) -> BTreeMap<String, Row> {
        let mut best: BTreeMap<String, (usize, Row)> = BTreeMap::new();
        for (idx, (name, win)) in view.iter().enumerate() {
            let Some(layer) = self.layers.get(name) else {
                continue;
            };
            for (id, versions) in layer {
                for row in versions {
                    if !win.admits(row.seq, at) {
                        continue;
                    }
                    let better = match best.get(id) {
                        None => true,
                        Some((best_idx, best_row)) => {
                            row.seq > best_row.seq || (row.seq == best_row.seq && idx < *best_idx)
                        }
                    };
                    if better {
                        best.insert(id.clone(), (idx, row.clone()));
                    }
                }
            }
        }
        best.into_iter().map(|(id, (_, row))| (id, row)).collect()
    }

    fn live(&self, view: &View, at: Option<Seq>) -> BTreeMap<String, String> {
        self.resolve(view, at)
            .into_iter()
            .filter(|(_, row)| row.assertive)
            .map(|(id, row)| (id, row.val))
            .collect()
    }

    /// Whether a key is live through a view, and the value it was last asserted with.
    fn key_state(&self, view: &View, id: &str) -> (bool, Option<String>) {
        let mut versions: Vec<(usize, &Row)> = vec![];
        for (idx, (name, win)) in view.iter().enumerate() {
            let Some(rows) = self.layers.get(name).and_then(|l| l.get(id)) else {
                continue;
            };
            for row in rows {
                if win.admits(row.seq, None) {
                    versions.push((idx, row));
                }
            }
        }
        // Newest first; on a tie the higher layer.
        versions.sort_by(|a, b| b.1.seq.cmp(&a.1.seq).then(a.0.cmp(&b.0)));
        let live = versions.first().map(|(_, r)| r.assertive).unwrap_or(false);
        let asserted = versions
            .iter()
            .find(|(_, r)| r.assertive)
            .map(|(_, r)| r.val.clone());
        (live, asserted)
    }

    /// The net effect of `src` materialized into `dst`'s top layer, at sequence `seq`.
    /// `Err` means the flatten must fail on a value collision.
    fn flatten(
        &mut self,
        src: &View,
        dst: &View,
        restamp: bool,
        seq: Seq,
    ) -> std::result::Result<ModelStats, String> {
        let net = self.resolve(src, None);
        let mut stats = ModelStats::default();
        let mut writes = vec![];
        for (id, row) in net {
            let (live, asserted) = self.key_state(dst, &id);
            if row.assertive {
                match asserted {
                    Some(ref existing) if *existing != row.val => {
                        return Err(format!("collision on {id}"))
                    }
                    Some(_) if live => {
                        stats.deduped += 1;
                        continue;
                    }
                    _ => writes.push((id, row)),
                }
            } else if live {
                writes.push((id, row));
            } else {
                stats.dropped += 1;
            }
        }
        let top = dst[0].0.clone();
        for (id, row) in writes {
            stats.copied += 1;
            self.write(
                &top,
                &id,
                Row {
                    seq: if restamp { seq } else { row.seq },
                    ..row
                },
            );
        }
        Ok(stats)
    }
}

/// The store and the model, driven in lockstep.
struct Pair {
    f: Fixture,
    model: Model,
    /// Every key's one immutable value: records are created and deleted, never updated.
    values: BTreeMap<String, String>,
    /// Fork points, so a branch's stack can be rebuilt at any time.
    forks: BTreeMap<String, Seq>,
    log: Vec<String>,
}

fn view_of(stack: &Stack) -> View {
    stack
        .iter()
        .map(|l| {
            (
                l.id.0.clone(),
                Win {
                    since: l.since,
                    bound: l.bound,
                },
            )
        })
        .collect()
}

impl Pair {
    fn new() -> Result<Pair> {
        Ok(Pair {
            f: Fixture::new()?,
            model: Model::default(),
            values: BTreeMap::new(),
            forks: BTreeMap::new(),
            log: vec![],
        })
    }

    fn branch(&mut self, name: &str) -> Result<Stack> {
        self.f.db.create_layer(name)?;
        let fork = self.f.seq();
        self.forks.insert(name.to_string(), fork);
        self.log.push(format!("branch {name} @ {fork}"));
        Ok(self.stack_of(name))
    }

    fn stack_of(&self, name: &str) -> Stack {
        if name == "default" {
            return base();
        }
        vec![
            LayerRef::new(name),
            LayerRef::bounded("default", self.forks[name]),
        ]
    }

    fn assert_rec(&mut self, layer: &str, id: &str) -> Result<()> {
        let stack = self.stack_of(layer);
        let val = self
            .values
            .entry(id.to_string())
            .or_insert_with(|| format!("val-of-{id}"))
            .clone();
        self.log.push(format!("assert {id} in {layer}"));
        let seq = self.f.assert_rec(&stack, id, &val)?;
        self.model.write(
            layer,
            id,
            Row {
                seq,
                assertive: true,
                val,
            },
        );
        Ok(())
    }

    fn retract_rec(&mut self, layer: &str, id: &str) -> Result<()> {
        let stack = self.stack_of(layer);
        self.log.push(format!("retract {id} in {layer}"));
        let seq = self.f.retract_rec(&stack, id)?;
        self.model.write(
            layer,
            id,
            Row {
                seq,
                assertive: false,
                val: String::new(),
            },
        );
        Ok(())
    }

    fn merge_down(&mut self, layer: &str) -> Result<()> {
        let src = vec![LayerRef::new(layer)];
        self.log.push(format!("merge {layer} down"));
        let stats = self.f.db.flatten(&src, &base(), true)?;
        let seq = stats
            .seq_range
            .expect("a restamping flatten reports its sequence")
            .0;
        let modelled = self
            .model
            .flatten(&view_of(&src), &view_of(&base()), true, seq)
            .map_err(|e| miette::miette!("the model expected this flatten to fail: {e}"))?;
        assert_eq!(
            (stats.rows_copied, stats.rows_deduped, stats.tombstones_dropped),
            (modelled.copied, modelled.deduped, modelled.dropped),
            "flatten stats diverged\n{}",
            self.log.join("\n")
        );
        Ok(())
    }

    /// Compare every stack the scenario can name, at the frontier and at a historical bound.
    fn check(&self, at: Option<Seq>) -> Result<()> {
        let mut stacks = vec![base()];
        for name in self.forks.keys() {
            stacks.push(self.stack_of(name));
        }
        for stack in stacks {
            let expected = self.model.live(&view_of(&stack), at);
            let actual = self.f.live(&stack, at)?;
            assert_eq!(
                actual,
                expected,
                "the store and the model disagree on stack {stack:?} at {at:?}\n{}",
                self.log.join("\n")
            );
        }
        Ok(())
    }
}

/// The randomized driver: arbitrary legal operation sequences against the store and the
/// model at once, with every read compared. The explicit cases cover the situations worth
/// naming; this covers the ones nobody thought to name.
#[test]
fn randomized_operations_match_the_model() -> Result<()> {
    for seed in 0..24u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut p = Pair::new()?;
        p.branch("a")?;
        p.branch("b")?;
        let layers = ["default", "a", "b"];
        let ids: Vec<String> = (0..6).map(|i| format!("k{i}")).collect();
        let mut checkpoints: Vec<Seq> = vec![];

        for step in 0..40 {
            let layer = layers[rng.gen_range(0..layers.len())];
            let id = &ids[rng.gen_range(0..ids.len())];
            match rng.gen_range(0..10) {
                0..=4 => p.assert_rec(layer, id)?,
                5..=7 => p.retract_rec(layer, id)?,
                8 => {
                    // Merging a branch down is legal at any point; the branch keeps its own
                    // fork, so it goes on reading its own view afterwards.
                    let branch = if rng.gen() { "a" } else { "b" };
                    p.merge_down(branch)?;
                }
                _ => checkpoints.push(p.f.seq()),
            }
            if step % 4 == 0 {
                p.check(None)?;
            }
        }

        p.check(None)?;
        for at in checkpoints {
            p.check(Some(at))?;
        }
    }
    Ok(())
}

/// Merging the same layer twice is merging it once.
#[test]
fn merging_is_idempotent() -> Result<()> {
    for seed in 0..8u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut p = Pair::new()?;
        p.branch("a")?;
        for _ in 0..12 {
            let id = format!("k{}", rng.gen_range(0..5));
            let layer = if rng.gen() { "default" } else { "a" };
            if rng.gen_range(0..3) == 0 {
                p.retract_rec(layer, &id)?;
            } else {
                p.assert_rec(layer, &id)?;
            }
        }
        p.merge_down("a")?;
        let once = p.f.live(&base(), None)?;
        p.merge_down("a")?;
        assert_eq!(p.f.live(&base(), None)?, once);
        p.check(None)?;
    }
    Ok(())
}

/// Merging two branches commutes whenever no key is retracted in one and asserted in the
/// other. With keys that are collision-free by construction, that is the ordinary case.
#[test]
fn disjoint_merges_commute() -> Result<()> {
    fn build(order: [&str; 2]) -> Result<BTreeMap<String, String>> {
        let f = Fixture::new()?;
        f.assert_rec(&base(), "shared", "v0")?;
        f.db.create_layer("a")?;
        f.db.create_layer("b")?;
        let fork = f.seq();
        let a: Stack = vec![LayerRef::new("a"), LayerRef::bounded("default", fork)];
        let b: Stack = vec![LayerRef::new("b"), LayerRef::bounded("default", fork)];

        f.assert_rec(&a, "only-a", "va")?;
        f.retract_rec(&a, "shared")?;
        f.assert_rec(&b, "only-b", "vb")?;

        for name in order {
            f.db.flatten(&vec![LayerRef::new(name)], &base(), true)?;
        }
        f.live(&base(), None)
    }

    assert_eq!(build(["a", "b"])?, build(["b", "a"])?);
    Ok(())
}

/// The documented non-property.
///
/// One branch retracts a key the base holds; the other re-asserts it. Merging A then B leaves
/// the key live; B then A leaves it dead, because B's assertion dedupes against a row that is
/// still live and then A's tombstone lands on it. Merge order is semantics, exactly as it is
/// in git. This test exists so that nobody "fixes" one ordering into the other.
#[test]
fn delete_and_reintroduce_merges_do_not_commute() -> Result<()> {
    fn build(order: [&str; 2]) -> Result<BTreeMap<String, String>> {
        let f = Fixture::new()?;
        f.assert_rec(&base(), "k", "v1")?;
        f.db.create_layer("deleter")?;
        f.db.create_layer("keeper")?;
        let fork = f.seq();
        let deleter: Stack = vec![LayerRef::new("deleter"), LayerRef::bounded("default", fork)];
        let keeper: Stack = vec![LayerRef::new("keeper"), LayerRef::bounded("default", fork)];

        f.retract_rec(&deleter, "k")?;
        f.assert_rec(&keeper, "k", "v1")?;

        for name in order {
            f.db.flatten(&vec![LayerRef::new(name)], &base(), true)?;
        }
        f.live(&base(), None)
    }

    // Deleter first: the keeper's assertion lands on a dead key and revives it.
    assert_eq!(build(["deleter", "keeper"])?, super::frontier(&[("k", "v1")]));
    // Keeper first: its assertion dedupes against the still-live row, so nothing is written,
    // and the deleter's tombstone then bites.
    assert_eq!(build(["keeper", "deleter"])?, super::frontier(&[]));
    Ok(())
}

/// Fork, work, merge down, drop is the same as doing the work directly on a quiescent base.
#[test]
fn a_fork_merge_round_trip_matches_direct_work() -> Result<()> {
    let direct = {
        let f = Fixture::new()?;
        f.assert_rec(&base(), "seeded", "v0")?;
        f.assert_rec(&base(), "added", "v1")?;
        f.assert_rec(&base(), "added2", "v2")?;
        f.retract_rec(&base(), "seeded")?;
        f.live(&base(), None)?
    };
    let forked = {
        let f = Fixture::new()?;
        f.assert_rec(&base(), "seeded", "v0")?;
        f.db.create_layer("work")?;
        let fork = f.seq();
        let stack: Stack = vec![LayerRef::new("work"), LayerRef::bounded("default", fork)];
        f.assert_rec(&stack, "added", "v1")?;
        f.assert_rec(&stack, "added2", "v2")?;
        f.retract_rec(&stack, "seeded")?;
        f.db.flatten(&vec![LayerRef::new("work")], &base(), true)?;
        f.db.drop_layer("work")?;
        f.live(&base(), None)?
    };
    assert_eq!(direct, forked);
    Ok(())
}

/// Picking a window and then reverting it leaves the frontier where it started. Tombstone
/// counts differ (the history remembers both moves), but the frontier must not.
#[test]
fn a_pick_and_its_revert_cancel() -> Result<()> {
    let f = Fixture::new()?;
    f.assert_rec(&base(), "existing", "v0")?;
    let start = f.seq();
    let before = f.live(&base(), None)?;

    f.db.create_layer("source")?;
    let source: Stack = vec![LayerRef::new("source"), LayerRef::bounded("default", start)];
    f.assert_rec(&source, "picked", "v1")?;
    f.retract_rec(&source, "existing")?;

    // Pick the whole branch into the base...
    f.db.flatten(&vec![LayerRef::new("source")], &base(), true)?;
    assert_eq!(f.live(&base(), None)?, super::frontier(&[("picked", "v1")]));

    // ...then revert it: retract what it asserted, re-assert what it retracted.
    for change in f.versions(&vec![LayerRef::new("source")])? {
        if change.assertive {
            f.retract_rec(&base(), &change.id)?;
        } else {
            let pre_image = f
                .versions(&vec![LayerRef::bounded("default", start)])?
                .into_iter()
                .find(|v| v.id == change.id && v.assertive)
                .expect("a retraction must have had something to retract");
            f.assert_rec(&base(), &change.id, &pre_image.val)?;
        }
    }

    assert_eq!(f.live(&base(), None)?, before);
    Ok(())
}
