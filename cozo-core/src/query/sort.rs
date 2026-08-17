/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::cmp::Ordering;
use std::collections::BTreeMap;

use itertools::Itertools;
use miette::Result;

use crate::data::memsize::est_tuple_bytes;
use crate::data::program::SortDir;
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::runtime::db::MemBudget;
use crate::runtime::temp_store::EpochStore;
use crate::runtime::transact::SessionTx;

impl<'a> SessionTx<'a> {
    pub(crate) fn sort_and_collect(
        &mut self,
        original: EpochStore,
        sorters: &[(Symbol, SortDir)],
        head: &[Symbol],
        mem: Option<&MemBudget>,
    ) -> Result<Vec<Tuple>> {
        let head_indices: BTreeMap<_, _> = head.iter().enumerate().map(|(i, k)| (k, i)).collect();
        let idx_sorters = sorters
            .iter()
            .map(|(k, dir)| (head_indices[k], *dir))
            .collect_vec();

        // memory budget (mnestic fork; spec `docs/specs/memory-budget.md`):
        // sorting copies the whole store into a Vec — the copy that doubles
        // peak memory on exactly the largest results — so it is charged, and
        // this is a fallible context, so it can error directly.
        let mut all_data: Vec<Tuple>;
        match mem {
            Some(m) => {
                all_data = Vec::new();
                for (i, v) in original.all_iter().enumerate() {
                    let t = v.into_tuple();
                    m.charge(est_tuple_bytes(&t));
                    if i & 4095 == 0 {
                        m.error_if_tripped()?;
                    }
                    all_data.push(t);
                }
                m.error_if_tripped()?;
            }
            None => {
                all_data = original.all_iter().map(|v| v.into_tuple()).collect_vec();
            }
        }
        all_data.sort_by(|a, b| {
            for (idx, dir) in &idx_sorters {
                match a[*idx].cmp(&b[*idx]) {
                    Ordering::Equal => {}
                    o => {
                        return match dir {
                            SortDir::Asc => o,
                            SortDir::Dsc => o.reverse(),
                        }
                    }
                }
            }
            Ordering::Equal
        });

        Ok(all_data)
    }
}
