/*
 * Layered storage test harness.
 *
 * Two conventions matter throughout:
 *
 * *Never hardcode sequence numbers.* They are sparse, because the counter advances on every
 * write to the instance and not only ours, so every expectation is derived from a stamp the
 * test read back
 * out of the store or captured from `current_seq()`.
 *
 * *Say which equivalence is meant.* `live()` compares frontiers: what a stack resolves to now.
 * `versions()` compares histories: every row, with its stamp. Restamping breaks history
 * equivalence by design, so tests that restamp compare frontiers only.
 */

use std::collections::BTreeMap;

use miette::Result;
use tempfile::TempDir;

use crate::data::value::DataValue;
use crate::runtime::db::ScriptMutability;
use crate::storage::layered::{new_cozo_layered, LayerRef, LayeredStorage, Seq, Stack};
use crate::Db;

mod concurrency;
mod crash;
mod flatten;
mod functional;
mod immutability;
mod indexes;
mod lifecycle;
mod oracle;
mod reads;
mod restack;
mod retraction;
mod sequences;
mod three_way;
mod windows;

/// One row as the store holds it: key, stamp, whether it asserts, and its value.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct Version {
    pub id: String,
    pub seq: Seq,
    pub assertive: bool,
    pub val: String,
}

pub(crate) struct Fixture {
    _dir: TempDir,
    pub db: Db<LayeredStorage>,
}

/// The default layer, unwindowed: the base of every fixture.
pub(crate) fn base() -> Stack {
    vec![LayerRef::new("default")]
}

impl Fixture {
    /// A store holding one stackable relation, `rec`: UUID-shaped string keys, opaque values.
    pub(crate) fn new() -> Result<Fixture> {
        let dir = TempDir::new().unwrap();
        let db = new_cozo_layered(dir.path())?;
        db.run_script(
            ":create rec {id: String, at: Validity => val: String}",
            Default::default(),
            ScriptMutability::Mutable,
        )?;
        Ok(Fixture { _dir: dir, db })
    }

    pub(crate) fn seq(&self) -> Seq {
        self.db.current_seq().unwrap()
    }

    fn run(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
        stack: &Stack,
        at: Option<Seq>,
        mutability: ScriptMutability,
    ) -> Result<crate::NamedRows> {
        self.db.run_on_stack(script, params, stack, at, mutability)
    }

    /// Create a record through `stack`. Returns the sequence it was stamped with.
    pub(crate) fn assert_rec(&self, stack: &Stack, id: &str, val: &str) -> Result<Seq> {
        let mut params = BTreeMap::new();
        params.insert("id".to_string(), DataValue::from(id));
        params.insert("val".to_string(), DataValue::from(val));
        self.run(
            "?[id, at, val] <- [[$id, 'ASSERT', $val]] :put rec {id, at => val}",
            params,
            stack,
            None,
            ScriptMutability::Mutable,
        )?;
        // Read the stamp back rather than computing it: the test must not encode the stamping
        // rule it is there to check.
        let newest = self
            .versions(stack)?
            .into_iter()
            .filter(|v| v.id == id)
            .map(|v| v.seq)
            .max()
            .expect("the row just written is not there");
        Ok(newest)
    }

    /// Retract a record through `stack`. Returns the sequence of the retraction.
    pub(crate) fn retract_rec(&self, stack: &Stack, id: &str) -> Result<Seq> {
        let mut params = BTreeMap::new();
        params.insert("id".to_string(), DataValue::from(id));
        self.run(
            "?[id, at, val] <- [[$id, 'RETRACT', '']] :put rec {id, at => val}",
            params,
            stack,
            None,
            ScriptMutability::Mutable,
        )?;
        let newest = self
            .versions(stack)?
            .into_iter()
            .filter(|v| v.id == id)
            .map(|v| v.seq)
            .max()
            .expect("the retraction just written is not there");
        Ok(newest)
    }

    /// The frontier through `stack`: what resolves live, optionally as of `at`.
    pub(crate) fn live(&self, stack: &Stack, at: Option<Seq>) -> Result<BTreeMap<String, String>> {
        let rows = self.run(
            "?[id, val] := *rec{id, val @ 'NOW'}",
            Default::default(),
            stack,
            at,
            ScriptMutability::Immutable,
        )?;
        Ok(rows
            .rows
            .into_iter()
            .map(|row| (as_str(&row[0]), as_str(&row[1])))
            .collect())
    }

    /// Every version visible through `stack`, retractions included: the history, not the frontier.
    pub(crate) fn versions(&self, stack: &Stack) -> Result<Vec<Version>> {
        let rows = self.run(
            "?[id, at, val] := *rec{id, at, val}",
            Default::default(),
            stack,
            None,
            ScriptMutability::Immutable,
        )?;
        let mut ret: Vec<Version> = rows
            .rows
            .into_iter()
            .map(|row| {
                let (seq, assertive) = match &row[1] {
                    DataValue::Validity(v) => (v.timestamp.0 .0, v.is_assert.0),
                    other => panic!("expected a validity, got {:?}", other),
                };
                Version {
                    id: as_str(&row[0]),
                    seq,
                    assertive,
                    val: as_str(&row[2]),
                }
            })
            .collect();
        ret.sort();
        Ok(ret)
    }
}

pub(crate) fn as_str(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        other => panic!("expected a string, got {:?}", other),
    }
}

/// `{"k" => "v", ...}`, for comparing against [`Fixture::live`].
pub(crate) fn frontier(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
