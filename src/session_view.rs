//! Independent read handles over a published, committed fold snapshot.
//!
//! The writer alone mutates the pipeline. Its short-lived reader factory
//! builds an owning view between jobs; serving that view never locks the
//! pipeline, even while a transaction is applying or flushing indexes.

use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use fold::pipeline::{Keyed, Push};
use fold::stream::{PipelineInitCtx, Readable, WriteTx};

use crate::dispatch::RootReaders;
use crate::model::{Id, Rec};

type ViewReaders<'a> = RootReaders<'a, fjall::Snapshot>;

self_cell::self_cell! {
    struct ViewCell {
        owner: fjall::Snapshot,
        #[covariant]
        dependent: ViewReaders,
    }
}

/// One published snapshot and its readers, plus the snapshot's node count
/// held in memory (never stored) so `figmog_status`/`figmog_files` answer
/// in O(1) per publication instead of decoding the whole `nodes` table on
/// every poll (NWS-2942).
pub(crate) struct SessionView {
    cell: ViewCell,
    node_count: OnceLock<usize>,
}

impl SessionView {
    /// Pin `snapshot` with readers built by `readers`. `node_count` is the
    /// snapshot's node count when the publisher knows it (a pull worker
    /// just synced exactly that many nodes); `None` leaves it for
    /// [`node_count`](Self::node_count) to count once.
    pub(crate) fn new(
        snapshot: fjall::Snapshot,
        readers: impl for<'a> FnOnce(&'a fjall::Snapshot) -> ViewReaders<'a>,
        node_count: Option<usize>,
    ) -> Self {
        let cell = ViewCell::new(snapshot, readers);
        let node_count = node_count.map(OnceLock::from).unwrap_or_default();
        SessionView { cell, node_count }
    }

    pub(crate) fn with_dependent<'a, T>(
        &'a self,
        f: impl FnOnce(&'a fjall::Snapshot, &'a ViewReaders<'a>) -> T,
    ) -> T {
        f(self.cell.borrow_owner(), self.cell.borrow_dependent())
    }

    /// The snapshot's node count: the publisher's, or counted here once
    /// (decoding every node) and cached for the view's lifetime. A store
    /// opened from disk is counted by its session worker right after
    /// opening, so polls don't pay for it.
    pub(crate) fn node_count(&self) -> usize {
        *self
            .node_count
            .get_or_init(|| self.with_dependent(|_, ((nodes, ..), ..)| nodes.iter().count()))
    }
}

/// Adapts fold's public pipeline API, retaining its exact sinks and writes.
/// The snapshot reference returned alongside the normal readers lets the
/// owner pin a new view after a complete pull, without reopening the store.
pub(crate) struct SharedPipeline<P>(pub(crate) Arc<Mutex<P>>);

impl<P: Push<Keyed<Id, Rec>>> Push<Keyed<Id, Rec>> for SharedPipeline<P> {
    type Reader<'tx, R: Readable + 'tx> = (&'tx R, P::Reader<'tx, R>);

    fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
        self.0.lock().unwrap().init(init);
    }

    fn push(&mut self, tx: &mut WriteTx<'_>, data: &Keyed<Id, Rec>, delta: isize) {
        self.0.lock().unwrap().push(tx, data, delta);
    }

    fn commit(&mut self, tx: &mut WriteTx<'_>) {
        self.0.lock().unwrap().commit(tx);
    }

    fn abort(&mut self) {
        // A failed pipeline callback can poison the mutex; fold still needs
        // to abort its buffered operators before unwinding the transaction.
        self.0.lock().unwrap_or_else(|e| e.into_inner()).abort();
        self.0.clear_poison();
    }

    fn reader<'tx, R: Readable>(&self, tx: &'tx R) -> Self::Reader<'tx, R> {
        (tx, self.0.lock().unwrap().reader(tx))
    }
}

/// Per-store admission gate for off-loop reads (NWS-2962), and the first
/// half of the per-store reset barrier: a worker read holds a
/// [`ReadLease`] — its own `Arc` of a published [`SessionView`] — for as
/// long as it runs. A store close or reset (1) validates its input, (2)
/// [`close`](Self::close)s this gate so no new read is admitted for that
/// store only, (3) waits until [`outstanding`](Self::outstanding) reaches
/// zero (polled by the serve loop, or [`wait_drained`](Self::wait_drained)
/// with a deadline at shutdown), and only then (4) resets/reopens the
/// store and (5) publishes a fresh session with an open gate. Other files'
/// gates are untouched, so their reads stay available throughout. A store
/// is never deleted or reopened underneath a live view.
#[derive(Default)]
pub(crate) struct ReadGate {
    state: Mutex<GateState>,
    released: Condvar,
}

#[derive(Default)]
struct GateState {
    closed: bool,
    outstanding: usize,
}

impl ReadGate {
    /// Admit one off-loop read of `view`, unless the gate is closed.
    pub(crate) fn admit(self: &Arc<Self>, view: &Arc<SessionView>) -> Option<ReadLease> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return None;
        }
        state.outstanding += 1;
        Some(ReadLease {
            view: Some(view.clone()),
            gate: self.clone(),
        })
    }

    /// Stop admitting reads (barrier step 2). Leases already granted run on.
    pub(crate) fn close(&self) {
        self.state.lock().unwrap().closed = true;
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// Reads admitted and not yet released.
    pub(crate) fn outstanding(&self) -> usize {
        self.state.lock().unwrap().outstanding
    }

    /// Block until every admitted read released its view, or `deadline`
    /// passes; `true` when drained.
    pub(crate) fn wait_drained(&self, deadline: Instant) -> bool {
        let mut state = self.state.lock().unwrap();
        while state.outstanding > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self.released.wait_timeout(state, left).unwrap().0;
        }
        true
    }
}

/// One admitted off-loop read: keeps its published view alive and counts
/// against its store's [`ReadGate`] until dropped. The view is released
/// before the gate is told, so a drained gate means no lease still pins a
/// snapshot.
pub(crate) struct ReadLease {
    view: Option<Arc<SessionView>>,
    gate: Arc<ReadGate>,
}

impl ReadLease {
    pub(crate) fn view(&self) -> &SessionView {
        self.view.as_ref().expect("held until drop")
    }
}

impl Drop for ReadLease {
    fn drop(&mut self) {
        self.view.take();
        let mut state = self.gate.state.lock().unwrap();
        state.outstanding -= 1;
        if state.outstanding == 0 {
            self.gate.released.notify_all();
        }
    }
}
