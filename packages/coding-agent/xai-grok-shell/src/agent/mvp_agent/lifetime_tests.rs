//! Purpose: Check the LocalRef owner contract on normal exit and unwind.
//! Input: A local task with a raw reference and a separate connection owner.
//! Output: Assertions that the task destructor runs before the value destructor.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use super::LocalRef;

struct ReferencedValue {
    marker: &'static str,
    events: Rc<RefCell<Vec<&'static str>>>,
}

impl Drop for ReferencedValue {
    fn drop(&mut self) {
        self.events.borrow_mut().push("value");
    }
}

struct TaskReference {
    reference: LocalRef<ReferencedValue>,
    live_value: Weak<ReferencedValue>,
}

impl Drop for TaskReference {
    fn drop(&mut self) {
        // Check liveness before the raw dereference so a regression cannot cause UB.
        assert!(self.live_value.upgrade().is_some(), "task outlived its owner");
        let value = self.reference.get();
        assert_eq!(value.marker, "agent allocation");
        value.events.borrow_mut().push("task");
    }
}

#[test]
fn local_ref_owner_outlives_local_set_on_exit() {
    assert_eq!(run_owner_probe(false), ["task", "value"]);
}

#[test]
fn local_ref_owner_outlives_local_set_on_unwind() {
    assert_eq!(run_owner_probe(true), ["task", "value"]);
}

/// Purpose: Exercise task destruction after the connection releases its agent.
/// Input: Whether the entrypoint must unwind after it starts the tasks.
/// Output: The destructor event order.
fn run_owner_probe(panic_during_run: bool) -> Vec<&'static str> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut keepalive = None;
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let value = Rc::new(ReferencedValue {
                marker: "agent allocation",
                events: Rc::clone(&events),
            });
            keepalive = Some(Rc::clone(&value));
            let task_reference = TaskReference {
                reference: LocalRef::new(value.as_ref()),
                live_value: Rc::downgrade(&value),
            };
            let connection = tokio::task::spawn_local(async move {
                let _value = value;
                std::future::pending::<()>().await;
            });
            tokio::task::spawn_local(async move {
                let _reference = task_reference;
                std::future::pending::<()>().await;
            });
            tokio::task::yield_now().await;
            connection.abort();
            assert!(connection.await.unwrap_err().is_cancelled());
            assert!(events.borrow().is_empty());
            if panic_during_run {
                panic!("entrypoint unwind");
            }
        });
        drop(local);
        drop(keepalive);
    }));
    assert_eq!(result.is_err(), panic_during_run);
    events.borrow().clone()
}
