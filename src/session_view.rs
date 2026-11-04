//! Independent read handles over a published, committed fold snapshot.
//!
//! The writer alone mutates the pipeline. Its short-lived reader factory
//! builds an owning view between jobs; serving that view never locks the
//! pipeline, even while a transaction is applying or flushing indexes.

use std::sync::{Arc, Mutex};

use fold::pipeline::{Keyed, Push};
use fold::stream::{PipelineInitCtx, Readable, WriteTx};

use crate::dispatch::RootReaders;
use crate::model::{Id, Rec};

type ViewReaders<'a> = RootReaders<'a, fjall::Snapshot>;

self_cell::self_cell! {
    pub(crate) struct SessionView {
        owner: fjall::Snapshot,
        #[covariant]
        dependent: ViewReaders,
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
