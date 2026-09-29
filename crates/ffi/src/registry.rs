//! The table that turns a handle into a value.
//!
//! A handle is an opaque `usize` the caller holds instead of a pointer. It
//! names an entry here, and a lookup is the only way to reach the value behind
//! it. That indirection is the whole point. A pointer that has been freed can
//! still be dereferenced, and what comes out of it is undefined behaviour; a
//! handle that has been released simply is not in the table. The worst a stale,
//! duplicated or made-up handle can produce is the answer a missing one gets,
//! which is an answer every caller already knows how to read.
//!
//! # Liveness
//!
//! An entry is inserted when the value is created and removed when the caller
//! releases it, so the table holds exactly the live handles. Handles are drawn
//! from a counter that never goes backwards, which is what makes the removal
//! safe: a released handle can never come to name a different value later,
//! because no handle is ever issued twice. That is the guarantee a
//! slot-and-generation table buys, reached without either — there are no slots
//! to recycle, so there is nothing for a generation to protect.
//!
//! The counter is shared by every table. Two tables numbering their entries
//! from the same place would hand the same number to a document and a grammar,
//! and a caller that passed the wrong one of the two would reach a value
//! instead of nothing. One counter costs a document nothing and keeps the two
//! apart.
//!
//! Zero is never issued, and is what "no handle" is spelled as on both sides.
//!
//! # Two locks, and they never nest
//!
//! The table's own lock is held only long enough to clone the `Arc` out of it;
//! no operation runs while it is held. The value's lock is taken afterwards and
//! held for the length of the operation. Taking them in that order and never
//! holding the first while taking the second is what keeps two threads from
//! deadlocking — and the value's lock is what keeps them from editing the same
//! document at once, which without it they could each do through a valid
//! `&mut`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

/// The next handle to issue, shared by every table.
///
/// Starts at 1 so that 0 stays free to mean "no handle". One counter for all of
/// them: see the module docs.
static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(1);

/// The live values of one kind, keyed by handle.
pub(crate) struct Registry<T> {
    entries: OnceLock<Mutex<HashMap<usize, Arc<Mutex<T>>>>>,
}

impl<T> Registry<T> {
    /// An empty table.
    ///
    /// A `const fn` so that each kind of value can have its own `static`
    /// without a lazy initialiser: the map is built on first use, and a table
    /// nobody touches is never allocated at all.
    pub(crate) const fn new() -> Self {
        Self {
            entries: OnceLock::new(),
        }
    }

    /// Stores `value` and returns the handle that names it.
    pub(crate) fn insert(&self, value: T) -> usize {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        // Unreachable outside a process that issued as many handles as there
        // are addresses. Checked because the alternative is not a crash but a
        // handle of 0, which everything else reads as "no handle".
        debug_assert!(handle != 0, "the handle space is exhausted");
        self.entries().insert(handle, Arc::new(Mutex::new(value)));
        handle
    }

    /// Releases the value `handle` names, answering with it.
    ///
    /// A handle that was never issued, or has already been released, answers
    /// `None` — which is what makes a double release harmless rather than a
    /// second free.
    pub(crate) fn remove(&self, handle: usize) -> Option<Arc<Mutex<T>>> {
        self.entries().remove(&handle)
    }

    /// Runs `f` on the value `handle` names.
    ///
    /// Answers `None` when there is no such live handle, and when the value's
    /// lock is poisoned — see below. An `f` that panics unwinds through here,
    /// so the value is left locked and poisoned for the next caller to find.
    pub(crate) fn with<R>(&self, handle: usize, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let value = self.acquire(handle)?;
        let mut guard = match value.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // A poisoned lock means an operation panicked while holding it,
                // so the value is half-way through whatever it was doing and
                // its state cannot be trusted. The handle is retired rather
                // than handed out again; the caller reads the failure the same
                // way it reads a handle that is not there.
                //
                // Dropped before the table is touched: the `Err` carries the
                // guard, and the two locks must not nest — see the module docs.
                drop(poisoned);
                drop(self.remove(handle));
                return None;
            }
        };
        Some(f(&mut guard))
    }

    /// Runs `f` on the value `handle` names, where `f` may itself decline.
    ///
    /// Answers `None` for either reason, because to the caller of an operation
    /// that can refuse they are the same reason: there is no answer to hand
    /// back. Spelled as its own method rather than left to the caller so that
    /// the two `None`s do not nest into a shape that reads like a mistake.
    pub(crate) fn and_then<R>(
        &self,
        handle: usize,
        f: impl FnOnce(&mut T) -> Option<R>,
    ) -> Option<R> {
        self.with(handle, f).flatten()
    }

    /// The value behind `handle`, or `None` when it is not live.
    fn acquire(&self, handle: usize) -> Option<Arc<Mutex<T>>> {
        // The table lock lives only as long as this statement: the `Arc` is
        // cloned out and the lock released before the caller locks the value.
        self.entries().get(&handle).cloned()
    }

    /// The table's own lock.
    fn entries(&self) -> MutexGuard<'_, HashMap<usize, Arc<Mutex<T>>>> {
        // Taken anyway when poisoned. This lock guards nothing but the map, and
        // the operations that touch the map — insert, remove, clone an `Arc` —
        // cannot panic part-way, so a panic here says nothing about what the
        // map holds. A poisoned *value* lock is the one that matters, and `with`
        // answers for it.
        self.entries
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    /// A table of its own, so one test's handles are not another's. The handles
    /// themselves still come from the shared counter, which is the point of the
    /// last test below.
    fn registry() -> Registry<u32> {
        Registry::new()
    }

    #[test]
    fn an_inserted_value_is_reachable_through_its_handle() {
        let table = registry();
        let handle = table.insert(7);
        assert_eq!(table.with(handle, |value| *value), Some(7));
    }

    #[test]
    fn an_operation_can_edit_the_value_in_place() {
        let table = registry();
        let handle = table.insert(1);
        table.with(handle, |value| *value += 41);
        assert_eq!(table.with(handle, |value| *value), Some(42));
    }

    #[test]
    fn a_handle_that_was_never_issued_finds_nothing() {
        let table = registry();
        assert_eq!(table.with(999, |value| *value), None);
    }

    #[test]
    fn zero_is_never_issued_and_finds_nothing() {
        let table = registry();
        assert_ne!(table.insert(1), 0);
        assert_eq!(table.with(0, |value| *value), None);
    }

    /// The property the table exists for: releasing a handle does not let a
    /// value inserted afterwards answer to it.
    #[test]
    fn a_released_handle_never_comes_to_name_another_value() {
        let table = registry();
        let first = table.insert(1);
        table.remove(first);
        let second = table.insert(2);

        assert_ne!(first, second);
        assert_eq!(table.with(first, |value| *value), None);
        assert_eq!(table.with(second, |value| *value), Some(2));
    }

    /// A refused operation and a handle that is not there are the same answer,
    /// which is what makes the entry points a single `match`.
    #[test]
    fn an_operation_that_declines_reads_like_a_handle_that_is_not_there() {
        let table = registry();
        let handle = table.insert(1);

        assert_eq!(table.and_then(handle, |_| None::<u32>), None);
        assert_eq!(table.and_then(999, |value| Some(*value)), None);
        assert_eq!(table.and_then(handle, |value| Some(*value)), Some(1));
    }

    #[test]
    fn releasing_twice_is_harmless() {
        let table = registry();
        let handle = table.insert(1);
        assert!(table.remove(handle).is_some());
        assert!(table.remove(handle).is_none());
    }

    /// A released handle answers even while other values are live, which is the
    /// case a use-after-free would have walked into.
    #[test]
    fn a_released_handle_finds_nothing_beside_a_live_one() {
        let table = registry();
        let live = table.insert(1);
        let released = table.insert(2);
        table.remove(released);

        assert_eq!(table.with(released, |value| *value), None);
        assert_eq!(table.with(live, |value| *value), Some(1));
    }

    /// Two tables must not number their entries from the same place, or a
    /// document handle would resolve in the grammar table — the mix-up the
    /// table exists to make harmless.
    #[test]
    fn handles_from_different_tables_do_not_collide() {
        let documents: Registry<u32> = Registry::new();
        let grammars: Registry<u32> = Registry::new();
        let document = documents.insert(1);
        let grammar = grammars.insert(2);

        assert_ne!(document, grammar);
        assert_eq!(grammars.with(document, |value| *value), None);
    }

    /// An operation that panicked left the value half-way through whatever it
    /// was doing, so the handle is retired rather than handed out again.
    ///
    /// The panic is caught rather than silenced: the hook that prints it is
    /// process-wide and the other tests run alongside this one.
    #[test]
    fn an_operation_that_panics_retires_the_handle() {
        let table = registry();
        let handle = table.insert(1);

        let panicked = catch_unwind(AssertUnwindSafe(|| {
            table.with(handle, |_| panic!("inside the operation"));
        }));
        assert!(panicked.is_err());

        assert_eq!(table.with(handle, |value| *value), None);
        assert!(table.remove(handle).is_none(), "already retired");
    }
}
