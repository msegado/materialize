// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Logic and types for all appends executed by the [`Coordinator`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use derivative::Derivative;
use tokio::sync::OwnedMutexGuard;

use mz_ore::task;
use mz_repr::{Diff, GlobalId, Row, Timestamp};
use mz_sql::plan::Plan;
use mz_stash::Append;
use mz_storage::protocol::client::Update;

use crate::catalog::BuiltinTableUpdate;
use crate::coord::timeline::WriteTimestamp;
use crate::coord::{Coordinator, Message, PendingTxn};
use crate::session::{Session, WriteOp};
use crate::util::ClientTransmitter;
use crate::ExecuteResponse;

#[derive(Debug)]
pub struct AdvanceLocalInput<T> {
    advance_to: T,
    ids: Vec<GlobalId>,
}

/// An operation that is deferred while waiting for a lock.
pub(crate) enum Deferred {
    Plan(DeferredPlan),
    GroupCommit,
}

/// This is the struct meant to be paired with [`Message::WriteLockGrant`], but
/// could theoretically be used to queue any deferred plan.
#[derive(Derivative)]
#[derivative(Debug)]
pub(crate) struct DeferredPlan {
    #[derivative(Debug = "ignore")]
    pub tx: ClientTransmitter<ExecuteResponse>,
    pub session: Session,
    pub plan: Plan,
}

/// A pending write transaction that will be committing during the next group commit.
pub(crate) struct PendingWriteTxn {
    /// List of all write operations within the transaction.
    pub(crate) writes: Vec<WriteOp>,
    /// Holds the coordinator's write lock.
    pub(crate) write_lock_guard: Option<OwnedMutexGuard<()>>,
    /// Inner transaction.
    pub(crate) pending_txn: PendingTxn,
}

impl PendingWriteTxn {
    fn has_write_lock(&self) -> bool {
        self.write_lock_guard.is_some()
    }
}

/// Enforces critical section invariants for functions that perform writes to
/// tables, e.g. `INSERT`, `UPDATE`.
///
/// If the provided session doesn't currently hold the write lock, attempts to
/// grant it. If the coord cannot immediately grant the write lock, defers
/// executing the provided plan until the write lock is available, and exits the
/// function.
///
/// # Parameters
/// - `$coord: &mut Coord`
/// - `$tx: ClientTransmitter<ExecuteResponse>`
/// - `mut $session: Session`
/// - `$plan_to_defer: Plan`
///
/// Note that making this a macro rather than a function lets us avoid taking
/// ownership of e.g. session and lets us unilaterally enforce the return when
/// deferring work.
#[macro_export]
macro_rules! guard_write_critical_section {
    ($coord:expr, $tx:expr, $session:expr, $plan_to_defer: expr) => {
        if !$session.has_write_lock() {
            if $coord.try_grant_session_write_lock(&mut $session).is_err() {
                $coord.defer_write(Deferred::Plan(DeferredPlan {
                    tx: $tx,
                    session: $session,
                    plan: $plan_to_defer,
                }));
                return;
            }
        }
    };
}

impl<S: Append + 'static> Coordinator<S> {
    /// Attempts to commit all pending write transactions in a group commit. If the timestamp
    /// chosen for the writes is not ahead of `now()`, then we can execute and commit the writes
    /// immediately. Otherwise we must wait for `now()` to advance past the timestamp chosen for the
    /// writes.
    #[tracing::instrument(level = "debug", skip(self))]
    pub(crate) async fn try_group_commit(&mut self) {
        if self.pending_writes.is_empty() {
            return;
        }

        // If we need to sleep below, then it's possible that some DDL may execute while we sleep.
        // In that case, the DDL will use some timestamp greater than or equal to the time that we
        // peeked, closing the peeked time and making it invalid for future writes. Therefore, we
        // must get a new valid timestamp everytime this method is called.
        // In the future we should include DDL in group commits, to avoid this issue. Then
        // `self.peek_local_write_ts()` can be removed. Instead we can call
        // `self.get_and_step_local_write_ts()` and safely use that value once we wake up.
        let timestamp = self.peek_local_ts();
        let now = (self.catalog.config().now)();
        if timestamp > now {
            // Cap retry time to 1s. In cases where the system clock has retreated by
            // some large amount of time, this prevents against then waiting for that
            // large amount of time in case the system clock then advances back to near
            // what it was.
            let remaining_ms = std::cmp::min(timestamp.saturating_sub(now), 1_000);
            let internal_cmd_tx = self.internal_cmd_tx.clone();
            task::spawn(|| "group_commit", async move {
                tokio::time::sleep(Duration::from_millis(remaining_ms)).await;
                internal_cmd_tx
                    .send(Message::GroupCommit)
                    .expect("sending to internal_cmd_tx cannot fail");
            });
        } else if self
            .pending_writes
            .iter()
            .any(|pending_write| pending_write.has_write_lock())
        {
            // If some transaction already holds the write lock, then we can execute a group
            // commit.
            self.group_commit().await;
        } else if let Ok(_guard) = Arc::clone(&self.write_lock).try_lock_owned() {
            // If no transaction holds the write lock, then we need to acquire it.
            self.group_commit().await;
        } else {
            // If some running transaction already holds the write lock, then one of the
            // following things will happen:
            //   1. The transaction will submit a write which will transfer the
            //      ownership of the lock to group commit and trigger another group
            //      group commit.
            //   2. The transaction will complete without submitting a write (abort,
            //      empty writes, etc) which will drop the lock. The deferred group
            //      commit will then acquire the lock and execute a group commit.
            self.defer_write(Deferred::GroupCommit);
        }
    }

    /// Commits all pending write transactions at the same timestamp. All pending writes will be
    /// combined into a single Append command and sent to STORAGE as a single batch. All writes will
    /// happen at the same timestamp and all involved tables will be advanced to some timestamp
    /// larger than the timestamp of the write.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn group_commit(&mut self) {
        if self.pending_writes.is_empty() {
            return;
        }

        // The value returned here still might be ahead of `now()` if `now()` has gone backwards at
        // any point during this method. We will still commit the write without waiting for `now()`
        // to advance. This is ok because the next batch of writes will trigger the wait loop in
        // `try_group_commit()` if `now()` hasn't advanced past the global timeline, preventing
        // an unbounded advancing of the global timeline ahead of `now()`.
        let WriteTimestamp {
            timestamp,
            advance_to,
        } = self.get_and_step_local_write_ts().await;
        let mut appends: HashMap<GlobalId, Vec<Update<Timestamp>>> =
            HashMap::with_capacity(self.pending_writes.len());
        let mut responses = Vec::with_capacity(self.pending_writes.len());
        for PendingWriteTxn {
            writes,
            write_lock_guard: _,
            pending_txn:
                PendingTxn {
                    client_transmitter,
                    response,
                    session,
                    action,
                },
        } in self.pending_writes.drain(..)
        {
            for WriteOp { id, rows } in writes {
                // If the table that some write was targeting has been deleted while the write was
                // waiting, then the write will be ignored and we respond to the client that the
                // write was successful. This is only possible if the write and the delete were
                // concurrent. Therefore, we are free to order the write before the delete without
                // violating any consistency guarantees.
                if self.catalog.try_get_entry(&id).is_some() {
                    let updates = rows
                        .into_iter()
                        .map(|(row, diff)| Update {
                            row,
                            diff,
                            timestamp,
                        })
                        .collect::<Vec<_>>();
                    appends.entry(id).or_default().extend(updates);
                }
            }
            responses.push((client_transmitter, response, session, action));
        }
        let appends = appends
            .into_iter()
            .map(|(id, updates)| (id, updates, advance_to))
            .collect();
        self.controller
            .storage_mut()
            .append(appends)
            .expect("invalid updates")
            .await
            .expect("One-shot shouldn't fail")
            .unwrap();
        for (client_transmitter, response, mut session, action) in responses {
            session.vars_mut().end_transaction(action);
            client_transmitter.send(response, session);
        }
    }

    /// Submit a write to be executed during the next group commit.
    pub(crate) fn submit_write(&mut self, pending_write_txn: PendingWriteTxn) {
        self.internal_cmd_tx
            .send(Message::GroupCommit)
            .expect("sending to internal_cmd_tx cannot fail");
        self.pending_writes.push(pending_write_txn);
    }

    #[tracing::instrument(level = "debug", skip_all, fields(updates = updates.len()))]
    pub(crate) async fn send_builtin_table_updates(&mut self, updates: Vec<BuiltinTableUpdate>) {
        // Most DDL queries cause writes to system tables. Unlike writes to user tables, system
        // table writes are not batched in a group commit. This is mostly due to the complexity
        // around checking for conflicting DDL at commit time. There is a possibility that if a user
        // is executing DDL at a rate faster than 1 query per millisecond, then the global timeline
        // will unboundedly advance past the system clock. This can cause future queries to block,
        // but will not affect correctness. Since this rate of DDL is unlikely, we are leaving DDL
        // related writes out of group commits for now.
        //
        // In the future we can add these write to group commit by:
        //  1. Checking for conflicts at commit time and aborting conflicting DDL.
        //  2. Delaying modifications to on-disk and in-memory catalog until commit time.
        let WriteTimestamp {
            timestamp,
            advance_to,
        } = self.get_and_step_local_write_ts().await;
        let mut appends: HashMap<GlobalId, Vec<(Row, Diff)>> = HashMap::new();
        for u in updates {
            appends.entry(u.id).or_default().push((u.row, u.diff));
        }
        for (_, updates) in &mut appends {
            differential_dataflow::consolidation::consolidate(updates);
        }
        appends.retain(|_key, updates| !updates.is_empty());
        let appends = appends
            .into_iter()
            .map(|(id, updates)| {
                let updates = updates
                    .into_iter()
                    .map(|(row, diff)| Update {
                        row,
                        diff,
                        timestamp,
                    })
                    .collect();
                (id, updates, advance_to)
            })
            .collect::<Vec<_>>();
        if !appends.is_empty() {
            self.controller
                .storage_mut()
                .append(appends)
                .expect("invalid updates")
                .await
                .expect("One-shot shouldn't fail")
                .unwrap();
        }
    }

    /// Enqueue requests to advance all local inputs (tables) to the current wall
    /// clock or at least a time greater than any previous table read (if wall
    /// clock has gone backward).
    pub(crate) async fn queue_local_input_advances(&mut self, advance_to: mz_repr::Timestamp) {
        self.internal_cmd_tx
            .send(Message::AdvanceLocalInput(AdvanceLocalInput {
                advance_to,
                ids: self
                    .catalog
                    .entries()
                    .filter_map(|e| {
                        if e.is_table() || e.is_storage_collection() {
                            Some(e.id())
                        } else {
                            None
                        }
                    })
                    .collect(),
            }))
            .expect("sending to internal_cmd_tx cannot fail");
    }

    /// Advance a local input (table). This downgrades the capability of a table,
    /// which means that it can no longer produce new data before this timestamp.
    #[tracing::instrument(level = "debug", skip_all, fields(num_tables = inputs.ids.len()))]
    pub(crate) async fn advance_local_inputs(
        &mut self,
        inputs: AdvanceLocalInput<mz_repr::Timestamp>,
    ) {
        let storage = self.controller.storage();
        let appends = inputs
            .ids
            .into_iter()
            .filter_map(|id| {
                if self.catalog.try_get_entry(&id).is_none()
                    || !storage
                        .collection(id)
                        .unwrap()
                        .write_frontier
                        .less_than(&inputs.advance_to)
                {
                    // Filter out tables that were dropped while waiting for advancement.
                    // Filter out tables whose upper is already advanced. This is not needed for
                    // correctness (advance_to and write_frontier should be equal here), just
                    // performance, as it's a no-op.
                    None
                } else {
                    Some((id, vec![], inputs.advance_to))
                }
            })
            .collect::<Vec<_>>();
        // Note: Do not await the result; let it drop (as we don't need to block on completion)
        // The error that could be return
        self.controller
            .storage_mut()
            .append(appends)
            .expect("Empty updates cannot be invalid");
    }

    /// Defers executing `deferred` until the write lock becomes available; waiting
    /// occurs in a green-thread, so callers of this function likely want to
    /// return after calling it.
    pub(crate) fn defer_write(&mut self, deferred: Deferred) {
        let id = match &deferred {
            Deferred::Plan(plan) => plan.session.conn_id().to_string(),
            Deferred::GroupCommit => "group_commit".to_string(),
        };
        self.write_lock_wait_group.push_back(deferred);

        let internal_cmd_tx = self.internal_cmd_tx.clone();
        let write_lock = Arc::clone(&self.write_lock);
        // TODO(guswynn): see if there is more relevant info to add to this name
        task::spawn(|| format!("defer_write:{id}"), async move {
            let guard = write_lock.lock_owned().await;
            internal_cmd_tx
                .send(Message::WriteLockGrant(guard))
                .expect("sending to internal_cmd_tx cannot fail");
        });
    }

    /// Attempts to immediately grant `session` access to the write lock or
    /// errors if the lock is currently held.
    pub(crate) fn try_grant_session_write_lock(
        &self,
        session: &mut Session,
    ) -> Result<(), tokio::sync::TryLockError> {
        Arc::clone(&self.write_lock).try_lock_owned().map(|p| {
            session.grant_write_lock(p);
        })
    }
}
