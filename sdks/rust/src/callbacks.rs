//! Internal structures for managing row and reducer callbacks.
//!
//! The SpacetimeDB Rust Client SDK embraces a callback-driven API,
//! where client authors register callbacks to later run in response to some event.
//!
//! Client authors may want to register multiple callbacks on the same event,
//! and then to remove specific callbacks while leaving others,
//! so we define a `CallbackId` type which uniquely identifies a registered callback,
//! and can be used to remove it.
//!
//! Callbacks may access the database context through an `EventContext`,
//! and may therefore add or remove callbacks on the same or other events,
//! query the client cache, add or remove subscriptions, and make many other mutations.
//! To prevent deadlocks or re-entrancy, the SDK arranges to defer all such mutations in a queue.
//!
//! This module is internal, and may incompatibly change without warning.

use crate::{client_cache::TableAppliedDiff, error::InternalError, spacetime_module::SpacetimeModule};
use bytes::Bytes;
use spacetimedb_data_structures::map::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::{
    any::Any,
    sync::atomic::{AtomicUsize, Ordering},
};

/// An identifier for a registered callback.
///
/// Registering a callback returns a `CallbackId`,
/// which can later be used to de-register the callback.
///
/// Exported because codegen needs to reference this type.
/// SDK users should not interact with [`CallbackId`] directly,
/// instead using specific generated callback ID types.
#[doc(hidden)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct CallbackId {
    id: usize,
}

impl CallbackId {
    /// We maintain a global monotonic counter of [`CallbackId`]s,
    /// even though we only need local uniqueness,
    /// because it's easier than keeping track of a bunch of different counters.
    pub(crate) fn get_next() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        CallbackId {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// Manages row callbacks for a `DbContext`/`DbConnection`.
pub struct DbCallbacks<M: SpacetimeModule> {
    /// Maps table name to a set of callbacks.
    table_callbacks: HashMap<&'static str, TableCallbacks<M>>,
}

impl<M: SpacetimeModule> Default for DbCallbacks<M> {
    fn default() -> Self {
        Self {
            table_callbacks: HashMap::default(),
        }
    }
}

impl<M: SpacetimeModule> DbCallbacks<M> {
    /// Called by generated DbUpdate dispatch after borrowed row dispatch ends.
    pub fn invoke_owned_initial_callbacks<Row: Any + Clone>(
        &mut self,
        table_name: &'static str,
        update: &mut crate::spacetime_module::TableUpdate<Row>,
        event: &M::EventContext,
    ) {
        if update.is_initial() {
            invoke_owned_initial_callbacks(&mut self.get_table_callbacks(table_name).on_initial, event, update);
        }
    }

    /// Get the [`TableCallbacks`] for the table `table_name`.
    pub(crate) fn get_table_callbacks(&mut self, table_name: &'static str) -> &mut TableCallbacks<M> {
        self.table_callbacks.entry(table_name).or_default()
    }

    /// Check registration without creating callback state for an unseen table.
    pub fn has_callbacks(&self, table_name: &'static str) -> bool {
        self.table_callbacks
            .get(table_name)
            .is_some_and(|callbacks| callbacks.has_callbacks.load(Ordering::Relaxed))
    }

    /// Invoke all row callbacks for rows modified by `applied_diff` for the table `table_name`.
    pub fn invoke_table_row_callbacks<Row: Any + Clone>(
        &mut self,
        table_name: &'static str,
        applied_diff: &TableAppliedDiff<Row>,
        event: &M::EventContext,
    ) {
        if !applied_diff.has_changes() {
            return;
        }
        let table_callbacks = self.get_table_callbacks(table_name);
        if !table_callbacks.on_initial.is_empty() {
            if let Some(initial_rows) = applied_diff.initial_rows() {
                invoke_initial_callbacks(&mut table_callbacks.on_initial, event, initial_rows);
                return;
            }
        }
        if !table_callbacks.on_insert.is_empty() {
            for row in applied_diff.inserts() {
                table_callbacks.invoke_on_insert(event, row);
            }
        }
        if !table_callbacks.on_delete.is_empty() {
            for row in applied_diff.deletes() {
                table_callbacks.invoke_on_delete(event, row);
            }
        }
        if !table_callbacks.on_update.is_empty() {
            for (del, ins) in applied_diff.updates() {
                table_callbacks.invoke_on_update(event, del, ins);
            }
        }
    }
}

/// An insert or delete callback for a row defined by the module `M`.
///
/// Rows are passed to callbacks as `&dyn Any`,
/// and a wrapper inserted by the SDK will downcast to the actual row type
/// before invoking the user-supplied function.
pub(crate) type RowCallback<M> = Box<dyn FnMut(&<M as SpacetimeModule>::EventContext, &dyn Any) + Send + 'static>;

type InsertCallbackMap<M> = HashMap<CallbackId, RowCallback<M>>;
type DeleteCallbackMap<M> = HashMap<CallbackId, RowCallback<M>>;

pub(crate) enum InitialCallbackKind<Context> {
    Cloned(Box<dyn FnMut(&Context, &dyn Any) + Send + 'static>),
    Borrowed(Box<dyn FnMut(&Context, &dyn Any) + Send + 'static>),
    Owned(Box<dyn FnMut(&Context, Box<dyn Any>) + Send + 'static>),
}

pub(crate) type InitialCallback<M> = InitialCallbackKind<<M as SpacetimeModule>::EventContext>;
type InitialCallbackMap<M> = HashMap<CallbackId, InitialCallback<M>>;

// Vec is intentional: the borrowed callback downcasts this exact container via
// Any. A temporary vector of rows or row references would defeat the purpose.
#[allow(clippy::ptr_arg)]
fn invoke_initial_callbacks<Context, Row: Any + Clone>(
    callbacks: &mut HashMap<CallbackId, InitialCallbackKind<Context>>,
    ctx: &Context,
    rows: &Vec<crate::spacetime_module::WithBsatn<Row>>,
) {
    // Preserve the legacy slice API, but materialize its clone only when there
    // is a legacy listener. Borrowed-only users never allocate a snapshot copy.
    let cloned = callbacks
        .values()
        .any(|callback| matches!(callback, InitialCallbackKind::Cloned(_)))
        .then(|| rows.iter().map(|row| row.row.clone()).collect::<Vec<_>>());
    for callback in callbacks.values_mut() {
        match callback {
            InitialCallbackKind::Cloned(callback) => callback(ctx, cloned.as_ref().unwrap()),
            InitialCallbackKind::Borrowed(callback) => callback(ctx, rows),
            InitialCallbackKind::Owned(_) => {} // dispatched after diff borrows end
        }
    }
}

fn invoke_owned_initial_callbacks<Context, Row: Any + Clone>(
    callbacks: &mut HashMap<CallbackId, InitialCallbackKind<Context>>,
    ctx: &Context,
    update: &mut crate::spacetime_module::TableUpdate<Row>,
) {
    if !update.is_initial() {
        return;
    }
    let mut remaining = callbacks
        .values()
        .filter(|callback| matches!(callback, InitialCallbackKind::Owned(_)))
        .count();
    if remaining == 0 {
        return;
    }
    #[cfg(not(feature = "client-cache"))]
    let rows = std::mem::take(&mut update.inserts)
        .into_iter()
        .map(|row| row.row)
        .collect::<Vec<_>>();
    // Preserve cache-enabled behavior: the cache/update may still own rows.
    #[cfg(feature = "client-cache")]
    let rows = update.inserts.iter().map(|row| row.row.clone()).collect::<Vec<_>>();
    let mut rows = Some(rows);
    for callback in callbacks.values_mut() {
        if let InitialCallbackKind::Owned(callback) = callback {
            remaining -= 1;
            if remaining == 0 {
                callback(ctx, Box::new(rows.take().unwrap()));
            } else {
                callback(ctx, Box::new(rows.as_ref().unwrap().clone()));
            }
        }
    }
}

#[cfg(test)]
mod initial_callback_tests {
    use super::*;
    use crate::{spacetime_module::WithBsatn, InitialRows};

    #[derive(Debug)]
    struct Row {
        value: String,
        clones: Arc<AtomicUsize>,
    }
    impl Clone for Row {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::Relaxed);
            Self {
                value: self.value.clone(),
                clones: Arc::clone(&self.clones),
            }
        }
    }
    fn rows(clones: &Arc<AtomicUsize>) -> Vec<WithBsatn<Row>> {
        (0..5)
            .map(|index| WithBsatn {
                bsatn: Bytes::new(),
                row: Row {
                    value: format!("row-{index}"),
                    clones: Arc::clone(clones),
                },
            })
            .collect()
    }

    #[test]
    fn borrowed_only_callbacks_observe_original_rows_with_zero_clones() {
        let clones = Arc::new(AtomicUsize::new(0));
        let rows = rows(&clones);
        let original = &rows[0].row as *const Row as usize;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut callbacks: HashMap<_, InitialCallbackKind<()>> = HashMap::default();
        for _ in 0..2 {
            let calls = Arc::clone(&calls);
            callbacks.insert(
                CallbackId::get_next(),
                InitialCallbackKind::Borrowed(Box::new(move |_, erased| {
                    let rows = erased.downcast_ref::<Vec<WithBsatn<Row>>>().unwrap();
                    let view = InitialRows::new(rows);
                    assert_eq!(view.len(), 5);
                    assert_eq!(view.iter().next().unwrap() as *const Row as usize, original);
                    assert_eq!(view.chunks(2).map(|chunk| chunk.len()).collect::<Vec<_>>(), [2, 2, 1]);
                    assert_eq!(
                        view.iter().map(|row| row.value.as_str()).collect::<Vec<_>>(),
                        ["row-0", "row-1", "row-2", "row-3", "row-4"]
                    );
                    calls.fetch_add(1, Ordering::Relaxed);
                })),
            );
        }
        invoke_initial_callbacks(&mut callbacks, &(), &rows);
        assert_eq!(clones.load(Ordering::Relaxed), 0);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn mixed_callbacks_keep_legacy_slice_compatibility_and_share_one_clone() {
        let clones = Arc::new(AtomicUsize::new(0));
        let rows = rows(&clones);
        let original = &rows[0].row as *const Row as usize;
        let pointers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut callbacks: HashMap<_, InitialCallbackKind<()>> = HashMap::default();
        let borrowed_id = CallbackId::get_next();
        callbacks.insert(
            borrowed_id,
            InitialCallbackKind::Borrowed(Box::new(move |_, erased| {
                let rows = erased.downcast_ref::<Vec<WithBsatn<Row>>>().unwrap();
                assert_eq!(&rows[0].row as *const Row as usize, original);
            })),
        );
        let mut legacy_ids = Vec::new();
        for _ in 0..2 {
            let pointers = Arc::clone(&pointers);
            let id = CallbackId::get_next();
            legacy_ids.push(id);
            callbacks.insert(
                id,
                InitialCallbackKind::Cloned(Box::new(move |_, erased| {
                    let rows = erased.downcast_ref::<Vec<Row>>().unwrap();
                    assert_eq!(rows.len(), 5);
                    assert_ne!(rows.as_ptr() as usize, original);
                    pointers.lock().unwrap().push(rows.as_ptr() as usize);
                })),
            );
        }
        invoke_initial_callbacks(&mut callbacks, &(), &rows);
        assert_eq!(clones.load(Ordering::Relaxed), rows.len());
        let observed = pointers.lock().unwrap();
        assert_eq!(observed[0], observed[1]);
        drop(observed);
        // Removing legacy listeners restores zero-clone dispatch.
        for id in legacy_ids {
            callbacks.remove(&id);
        }
        invoke_initial_callbacks(&mut callbacks, &(), &rows);
        assert_eq!(clones.load(Ordering::Relaxed), rows.len());
        callbacks.remove(&borrowed_id);
        assert!(callbacks.is_empty());
    }

    #[test]
    fn empty_initial_snapshots_still_invoke_borrowed_callbacks() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let mut callbacks: HashMap<_, InitialCallbackKind<()>> = HashMap::default();
        callbacks.insert(
            CallbackId::get_next(),
            InitialCallbackKind::Borrowed(Box::new(move |_, erased| {
                let rows = erased.downcast_ref::<Vec<WithBsatn<Row>>>().unwrap();
                assert!(InitialRows::new(rows).is_empty());
                observed.fetch_add(1, Ordering::Relaxed);
            })),
        );
        invoke_initial_callbacks::<_, Row>(&mut callbacks, &(), &Vec::new());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    fn initial(rows: Vec<WithBsatn<Row>>) -> crate::spacetime_module::TableUpdate<Row> {
        let mut update = crate::spacetime_module::TableUpdate::default();
        let mut part = crate::spacetime_module::TableUpdate::default();
        part.inserts = rows;
        update.append(part);
        update.mark_initial();
        update
    }

    #[test]
    fn single_owned_listener_moves_payloads_in_cacheless_mode() {
        let clones = Arc::new(AtomicUsize::new(0));
        let mut update = initial(rows(&clones));
        let payload = update.inserts[0].row.value.as_ptr() as usize;
        let received = Arc::new(std::sync::Mutex::new(None));
        let output = Arc::clone(&received);
        let mut callbacks: HashMap<_, InitialCallbackKind<()>> = HashMap::default();
        callbacks.insert(
            CallbackId::get_next(),
            InitialCallbackKind::Owned(Box::new(move |_, erased| {
                let rows = *erased.downcast::<Vec<Row>>().unwrap();
                if !cfg!(feature = "client-cache") {
                    assert_eq!(rows[0].value.as_ptr() as usize, payload);
                }
                *output.lock().unwrap() = Some(rows);
            })),
        );
        // This phase must not call the owned listener, and must not clone rows.
        invoke_initial_callbacks(&mut callbacks, &(), &update.inserts);
        assert!(received.lock().unwrap().is_none());
        assert_eq!(clones.load(Ordering::Relaxed), 0);
        invoke_owned_initial_callbacks(&mut callbacks, &(), &mut update);
        assert_eq!(
            clones.load(Ordering::Relaxed),
            if cfg!(feature = "client-cache") { 5 } else { 0 }
        );
        assert_eq!(update.inserts.len(), if cfg!(feature = "client-cache") { 5 } else { 0 });
        drop(update);
        let received = received.lock().unwrap();
        let rows = received.as_ref().unwrap();
        assert_eq!(
            rows.iter().map(|row| row.value.as_str()).collect::<Vec<_>>(),
            ["row-0", "row-1", "row-2", "row-3", "row-4"]
        );
    }

    #[test]
    fn multiple_owned_listeners_each_receive_complete_snapshots() {
        let clones = Arc::new(AtomicUsize::new(0));
        let mut update = initial(rows(&clones));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut callbacks: HashMap<_, InitialCallbackKind<()>> = HashMap::default();
        for _ in 0..3 {
            let calls = Arc::clone(&calls);
            callbacks.insert(
                CallbackId::get_next(),
                InitialCallbackKind::Owned(Box::new(move |_, erased| {
                    let rows = *erased.downcast::<Vec<Row>>().unwrap();
                    assert_eq!(rows.len(), 5);
                    assert_eq!(rows[4].value, "row-4");
                    calls.fetch_add(1, Ordering::Relaxed);
                })),
            );
        }
        invoke_owned_initial_callbacks(&mut callbacks, &(), &mut update);
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(
            clones.load(Ordering::Relaxed),
            if cfg!(feature = "client-cache") { 15 } else { 10 }
        );
    }

    #[test]
    fn borrowed_and_legacy_callbacks_finish_before_owned_dispatch() {
        let clones = Arc::new(AtomicUsize::new(0));
        let mut update = initial(rows(&clones));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut callbacks: HashMap<_, InitialCallbackKind<()>> = HashMap::default();
        for borrowed in [true, false] {
            let calls = Arc::clone(&calls);
            let callback = Box::new(move |_: &(), rows: &dyn Any| {
                if borrowed {
                    assert_eq!(rows.downcast_ref::<Vec<WithBsatn<Row>>>().unwrap().len(), 5);
                } else {
                    assert_eq!(rows.downcast_ref::<Vec<Row>>().unwrap().len(), 5);
                }
                calls.fetch_add(1, Ordering::Relaxed);
            });
            let callback = if borrowed {
                InitialCallbackKind::Borrowed(callback)
            } else {
                InitialCallbackKind::Cloned(callback)
            };
            callbacks.insert(CallbackId::get_next(), callback);
        }
        let observed = Arc::clone(&calls);
        callbacks.insert(
            CallbackId::get_next(),
            InitialCallbackKind::Owned(Box::new(move |_, erased| {
                assert_eq!(observed.load(Ordering::Relaxed), 2);
                assert_eq!(erased.downcast::<Vec<Row>>().unwrap().len(), 5);
                observed.fetch_add(1, Ordering::Relaxed);
            })),
        );
        invoke_initial_callbacks(&mut callbacks, &(), &update.inserts);
        invoke_owned_initial_callbacks(&mut callbacks, &(), &mut update);
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(
            clones.load(Ordering::Relaxed),
            if cfg!(feature = "client-cache") { 10 } else { 5 }
        );
    }

    #[test]
    fn owned_listeners_receive_empty_and_repeated_snapshots_but_not_live_updates() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let mut callbacks: HashMap<_, InitialCallbackKind<()>> = HashMap::default();
        let id = CallbackId::get_next();
        callbacks.insert(
            id,
            InitialCallbackKind::Owned(Box::new(move |_, erased| {
                assert!(erased.downcast::<Vec<Row>>().unwrap().is_empty());
                observed.fetch_add(1, Ordering::Relaxed);
            })),
        );
        for _ in 0..2 {
            invoke_owned_initial_callbacks(&mut callbacks, &(), &mut initial(Vec::new()));
        }
        let clones = Arc::new(AtomicUsize::new(0));
        let mut live = crate::spacetime_module::TableUpdate::default();
        live.inserts = rows(&clones);
        invoke_owned_initial_callbacks(&mut callbacks, &(), &mut live);
        assert_eq!(live.inserts.len(), 5);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        callbacks.remove(&id);
        let mut update = initial(rows(&clones));
        invoke_owned_initial_callbacks(&mut callbacks, &(), &mut update);
        assert_eq!(update.inserts.len(), 5);
        assert_eq!(clones.load(Ordering::Relaxed), 0);
    }
}

/// An update callback for a row defined by the module `M`.
///
/// Rows are passed to callbacks as `&dyn Any`,
/// and a wrapper inserted by the SDK will downcast to the actual row type
/// before invoking the user-supplied function.
pub(crate) type UpdateCallback<M> =
    Box<dyn FnMut(&<M as SpacetimeModule>::EventContext, &dyn Any, &dyn Any) + Send + 'static>;

type UpdateCallbackMap<M> = HashMap<CallbackId, UpdateCallback<M>>;

/// A set of insert, delete and update callbacks for a particular table defined by the module `M`.
///
/// We store a set of update callbacks for all tables, even those which do not have a primary key field.
/// The public codegen interface makes it statically impossible to register or invoke such a callback.
pub(crate) struct TableCallbacks<M: SpacetimeModule> {
    on_initial: InitialCallbackMap<M>,
    on_insert: InsertCallbackMap<M>,
    on_delete: DeleteCallbackMap<M>,
    on_update: UpdateCallbackMap<M>,
    has_callbacks: Arc<AtomicBool>,
}

impl<M: SpacetimeModule> Default for TableCallbacks<M> {
    fn default() -> Self {
        Self {
            on_initial: Default::default(),
            on_insert: Default::default(),
            on_delete: Default::default(),
            on_update: Default::default(),
            has_callbacks: Arc::new(AtomicBool::default()),
        }
    }
}

impl<M: SpacetimeModule> TableCallbacks<M> {
    pub(crate) fn register_on_initial(&mut self, callback_id: CallbackId, callback: InitialCallback<M>) {
        self.on_initial.insert(callback_id, callback);
        self.has_callbacks.store(true, Ordering::SeqCst);
    }

    pub(crate) fn register_on_insert(&mut self, callback_id: CallbackId, callback: RowCallback<M>) {
        self.on_insert.insert(callback_id, callback);
        self.has_callbacks.store(true, Ordering::SeqCst);
    }

    pub(crate) fn register_on_delete(&mut self, callback_id: CallbackId, callback: RowCallback<M>) {
        self.on_delete.insert(callback_id, callback);
        self.has_callbacks.store(true, Ordering::SeqCst);
    }

    pub(crate) fn register_on_update(&mut self, callback_id: CallbackId, callback: UpdateCallback<M>) {
        self.on_update.insert(callback_id, callback);
        self.has_callbacks.store(true, Ordering::SeqCst);
    }

    pub(crate) fn check_if_still_has_callbacks(&self) -> bool {
        !self.on_initial.is_empty()
            || !self.on_insert.is_empty()
            || !self.on_delete.is_empty()
            || !self.on_update.is_empty()
    }

    pub(crate) fn remove_on_insert(&mut self, callback_id: CallbackId) {
        // Ugly: `impl FnMut` is `must_use`.
        // If we don't `.expect` this, no diagnostic,
        // but we want to assert that we actually removed a callback,
        // we just don't want to invoke it.
        // So we have to `let _ =`.
        let _ = self
            .on_insert
            .remove(&callback_id)
            .expect("Attempt to remove non-existent insert callback");

        if !self.check_if_still_has_callbacks() {
            self.has_callbacks.store(false, Ordering::SeqCst);
        }
    }

    pub(crate) fn remove_on_initial(&mut self, callback_id: CallbackId) {
        let _ = self
            .on_initial
            .remove(&callback_id)
            .expect("Attempt to remove non-existent initial callback");

        if !self.check_if_still_has_callbacks() {
            self.has_callbacks.store(false, Ordering::SeqCst);
        }
    }

    pub(crate) fn remove_on_delete(&mut self, callback_id: CallbackId) {
        // Ugly: `impl FnMut` is `must_use`.
        // If we don't `.expect` this, no diagnostic,
        // but we want to assert that we actually removed a callback,
        // we just don't want to invoke it.
        // So we have to `let _ =`.
        let _ = self
            .on_delete
            .remove(&callback_id)
            .expect("Attempt to remove non-existent delete callback");

        if !self.check_if_still_has_callbacks() {
            self.has_callbacks.store(false, Ordering::SeqCst);
        }
    }

    pub(crate) fn remove_on_update(&mut self, callback_id: CallbackId) {
        // Ugly: `impl FnMut` is `must_use`.
        // If we don't `.expect` this, no diagnostic,
        // but we want to assert that we actually removed a callback,
        // we just don't want to invoke it.
        // So we have to `let _ =`.
        let _ = self
            .on_update
            .remove(&callback_id)
            .expect("Attempt to remove non-existent update callback");

        if !self.check_if_still_has_callbacks() {
            self.has_callbacks.store(false, Ordering::SeqCst);
        }
    }

    fn invoke_on_insert(&mut self, ctx: &M::EventContext, row: &dyn Any) {
        for callback in self.on_insert.values_mut() {
            callback(ctx, row);
        }
    }

    fn invoke_on_delete(&mut self, ctx: &M::EventContext, row: &dyn Any) {
        for callback in self.on_delete.values_mut() {
            callback(ctx, row);
        }
    }

    fn invoke_on_update(&mut self, ctx: &M::EventContext, old: &dyn Any, new: &dyn Any) {
        for callback in self.on_update.values_mut() {
            callback(ctx, old, new);
        }
    }
}

/// A reducer callback for a reducer defined by the module `M`.
///
/// Reducer arguments are passed to callbacks within the `EventContext`,
/// and a wrapper inserted by the SDK will destructure the contained `Event`
/// before invoking the user-supplied function.
pub(crate) type ReducerCallback<M> = Box<
    dyn FnOnce(&<M as SpacetimeModule>::ReducerEventContext, Result<Result<(), String>, InternalError>)
        + Send
        + 'static,
>;

/// A collection of reducer callbacks.
///
/// References to this struct are autogenerated in the `handle_event`
/// function. Users should not reference this struct directly.
pub(crate) struct ReducerCallbacks<M: SpacetimeModule> {
    /// Maps WebSocket `request_id` to the arguments of that call and user-supplied callback.
    callbacks: HashMap<u32, (M::Reducer, ReducerCallback<M>)>,
}

impl<M: SpacetimeModule> Default for ReducerCallbacks<M> {
    fn default() -> Self {
        Self {
            callbacks: Default::default(),
        }
    }
}

impl<M: SpacetimeModule> ReducerCallbacks<M> {
    pub(crate) fn pop_call_info(&mut self, request_id: u32) -> Option<(M::Reducer, ReducerCallback<M>)> {
        self.callbacks.remove(&request_id)
    }

    pub(crate) fn store_call_info(&mut self, request_id: u32, args: M::Reducer, callback: ReducerCallback<M>) {
        if self.callbacks.insert(request_id, (args, callback)).is_some() {
            panic!("Re-used `request_id` {request_id} for multiple in-flight reducer requests.");
        }
    }
}

/// A procedure callback for a procedure defined by the module `M`.
///
/// Procedure return values are deserialized within this function by code injected by the SDK.
pub(crate) type ProcedureCallback<M> =
    Box<dyn FnOnce(&<M as SpacetimeModule>::ProcedureEventContext, Result<Bytes, InternalError>) + Send + 'static>;

pub struct ProcedureCallbacks<M: SpacetimeModule> {
    request_id_to_callback: HashMap<u32, ProcedureCallback<M>>,
}

impl<M: SpacetimeModule> Default for ProcedureCallbacks<M> {
    fn default() -> Self {
        Self {
            request_id_to_callback: Default::default(),
        }
    }
}

impl<M: SpacetimeModule> ProcedureCallbacks<M> {
    pub(crate) fn insert(&mut self, request_id: u32, callback: ProcedureCallback<M>) {
        if self.request_id_to_callback.insert(request_id, callback).is_some() {
            unreachable!("Request IDs are drawn from a global monotonic atomic counter and so are unique");
        };
    }

    pub(crate) fn resolve(
        &mut self,
        ctx: &<M as SpacetimeModule>::ProcedureEventContext,
        request_id: u32,
        result: Result<Bytes, InternalError>,
    ) {
        let callback = self
            .request_id_to_callback
            .remove(&request_id)
            .expect("Attempting to resolve a non-existent procedure callback");
        callback(ctx, result)
    }
}
