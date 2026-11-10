//! Per-terminal locks. A tool that types or presses keys into a pane holds the
//! lock for the pane's backend id for its whole sequence, so two Shepherd calls
//! on one pane never interleave. Callers lock the resolved id, not the name they
//! were given, so a name and its id share one lock.
//!
//! This serializes Shepherd's own calls only. A person at the terminal or another
//! client of the terminal backend is not covered.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

#[derive(Debug, Default)]
struct Table {
    held: HashSet<String>,
    /// Address -> calls blocked in `lock` for it. Entries are removed at zero.
    waiting: HashMap<String, usize>,
}

#[derive(Debug, Default)]
pub struct PaneLocks {
    table: Mutex<Table>,
    freed: Condvar,
}

/// Holds one address until dropped.
#[derive(Debug)]
pub struct PaneGuard {
    locks: Arc<PaneLocks>,
    address: String,
}

impl PaneLocks {
    fn table(&self) -> MutexGuard<'_, Table> {
        // The table holds no invariant a panic could break, so a poisoned lock is
        // still usable.
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Block until `address` is free, then hold it.
    pub fn lock(self: &Arc<Self>, address: &str) -> PaneGuard {
        let mut table = self.table();
        if table.held.contains(address) {
            *table.waiting.entry(address.to_string()).or_insert(0) += 1;
            while table.held.contains(address) {
                table = self
                    .freed
                    .wait(table)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if let Some(count) = table.waiting.get_mut(address) {
                *count -= 1;
                if *count == 0 {
                    table.waiting.remove(address);
                }
            }
        }
        table.held.insert(address.to_string());
        PaneGuard {
            locks: Arc::clone(self),
            address: address.to_string(),
        }
    }

    /// How many calls are blocked waiting for `address`. Tests use it to know a
    /// call is parked on the lock without sleeping.
    #[cfg(test)]
    pub fn waiting(&self, address: &str) -> usize {
        self.table().waiting.get(address).copied().unwrap_or(0)
    }

    /// Whether some call holds `address` now.
    #[cfg(test)]
    pub fn is_held(&self, address: &str) -> bool {
        self.table().held.contains(address)
    }
}

impl Drop for PaneGuard {
    fn drop(&mut self) {
        self.locks.table().held.remove(&self.address);
        self.locks.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn second_lock_on_same_address_waits_for_release() {
        let locks = Arc::new(PaneLocks::default());
        let first = locks.lock("t8");
        let (tx, rx) = mpsc::channel();
        let worker = {
            let locks = Arc::clone(&locks);
            thread::spawn(move || {
                let _second = locks.lock("t8");
                tx.send(()).unwrap();
            })
        };
        while locks.waiting("t8") == 0 {
            thread::yield_now();
        }
        assert!(rx.try_recv().is_err(), "second lock taken while first held");
        // A different address is not blocked.
        drop(locks.lock("t9"));
        drop(first);
        rx.recv().unwrap();
        worker.join().unwrap();
        assert_eq!(locks.waiting("t8"), 0);
        assert!(locks.table().held.is_empty());
    }
}
