/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A host keeps admission until the governed transaction's worker actually exits.
//! Run with `cargo run -p mnestic --example governed_transaction`.
use cozo::{DbInstance, GovernedTransactionOptions};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct AdmissionPermit(Arc<AtomicBool>);
impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn main() -> miette::Result<()> {
    let db = DbInstance::new("mem", "", "")?;
    db.run_default(":create items {id: Int => value: String}")?;
    let admitted = Arc::new(AtomicBool::new(true));
    let permit = AdmissionPermit(admitted.clone());
    let options = GovernedTransactionOptions::new(Instant::now() + Duration::from_secs(10));
    let (mut client, worker) = db.governed_transaction(true, options)?;
    let worker = std::thread::spawn(move || {
        let _permit = permit;
        worker.run();
    });
    // Joining is required on query errors too; dropping the captured client
    // requests cancellation, while the worker continues to own admission.
    let outcome = (|| -> miette::Result<()> {
        client.run_script(
            "?[id,value] <- [[1,'committed']] :put items {id=>value}",
            BTreeMap::new(),
        )?;
        assert!(admitted.load(Ordering::SeqCst));
        // A successful commit can precede worker-owned callback delivery.
        // A timeout racing commit requires application-level reconciliation.
        client.commit()
    })();
    worker.join().expect("governed worker panicked");
    outcome?;
    assert!(!admitted.load(Ordering::SeqCst));
    let rows = db.run_default("?[value] := *items{id:1,value}")?;
    assert_eq!(rows.rows[0][0].get_str(), Some("committed"));
    Ok(())
}
