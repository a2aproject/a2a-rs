// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
mod inmemory;
mod store;

pub use inmemory::InMemoryTaskStore;
pub use store::{StoredTask, TaskStore, TaskVersion};

use a2a::Task;

/// Truncate `task.history` to the last `history_length` messages.
///
/// `None` leaves history unchanged. `Some(n)` with `n <= 0` clears it.
/// Negative values must not be cast to `usize` (that wraps and returns the
/// full history).
pub(crate) fn apply_history_length(task: &mut Task, history_length: Option<i32>) {
    let Some(requested) = history_length else {
        return;
    };
    let keep = if requested <= 0 {
        0
    } else {
        requested as usize
    };
    let Some(history) = task.history.as_mut() else {
        return;
    };
    if keep == 0 {
        history.clear();
        return;
    }
    if history.len() > keep {
        history.drain(..history.len() - keep);
    }
}
