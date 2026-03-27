use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

type BoxFuture = Pin<Box<dyn Future<Output = ()>>>;

thread_local! {
    static SPAWN_QUEUE: RefCell<VecDeque<BoxFuture>> = RefCell::new(VecDeque::new());
}

/// Spawn a future onto the executor. Must be called from within `block_on`.
pub fn spawn(task: impl Future<Output = ()> + 'static) {
    SPAWN_QUEUE.with(|queue| {
        queue.borrow_mut().push_back(Box::pin(task));
    });
}

/// Yield control back to the executor so other tasks can run.
pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

pub struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            Poll::Pending
        }
    }
}

/// Run the main future to completion, driving all spawned tasks cooperatively.
///
/// Tasks that encounter unavailable I/O return `Poll::Pending`, allowing the
/// executor to poll other tasks. A brief 1ms sleep prevents busy-spinning
/// when all tasks are idle.
pub fn block_on(main: impl Future<Output = ()> + 'static) {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut tasks: Vec<BoxFuture> = vec![Box::pin(main)];

    loop {
        SPAWN_QUEUE.with(|queue| {
            let mut q = queue.borrow_mut();
            while let Some(task) = q.pop_front() {
                tasks.push(task);
            }
        });

        if tasks.is_empty() {
            break;
        }

        let mut made_progress = false;

        tasks.retain_mut(|task| match task.as_mut().poll(&mut cx) {
            Poll::Ready(()) => {
                made_progress = true;
                false
            }
            Poll::Pending => true,
        });

        if !made_progress {
            idle_sleep();
        }
    }
}

#[cfg(target_os = "wasi")]
fn idle_sleep() {
    let pollable = wasi::clocks::monotonic_clock::subscribe_duration(1_000_000); // 1ms
    pollable.block();
}

#[cfg(not(target_os = "wasi"))]
fn idle_sleep() {
    std::thread::sleep(std::time::Duration::from_millis(1));
}

fn noop_waker() -> Waker {
    fn no_op(_: *const ()) {}
    fn clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &VTABLE)
    }
    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn yield_now_returns_pending_then_ready() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut future = yield_now();
        let pinned = Pin::new(&mut future);

        assert_eq!(pinned.poll(&mut cx), Poll::Pending);

        let pinned = Pin::new(&mut future);
        assert_eq!(pinned.poll(&mut cx), Poll::Ready(()));
    }

    #[test]
    fn block_on_runs_immediate_future() {
        let done = Rc::new(Cell::new(false));
        let done_clone = Rc::clone(&done);

        block_on(async move {
            done_clone.set(true);
        });

        assert!(done.get());
    }

    #[test]
    fn block_on_runs_yielding_future() {
        let count = Rc::new(Cell::new(0u32));
        let count_clone = Rc::clone(&count);

        block_on(async move {
            count_clone.set(count_clone.get() + 1);
            yield_now().await;
            count_clone.set(count_clone.get() + 1);
            yield_now().await;
            count_clone.set(count_clone.get() + 1);
        });

        assert_eq!(count.get(), 3);
    }

    #[test]
    fn spawn_runs_child_tasks() {
        let results = Rc::new(RefCell::new(Vec::new()));
        let r1 = Rc::clone(&results);
        let r2 = Rc::clone(&results);

        block_on(async move {
            r1.borrow_mut().push("main-start");

            let r_child = Rc::clone(&r2);
            spawn(async move {
                r_child.borrow_mut().push("child");
            });

            // Yield so the executor picks up the spawned task
            yield_now().await;
            r1.borrow_mut().push("main-end");
        });

        let results = results.borrow();
        assert!(results.contains(&"main-start"));
        assert!(results.contains(&"child"));
        assert!(results.contains(&"main-end"));
    }

    #[test]
    fn spawn_multiple_tasks_all_complete() {
        let counter = Rc::new(Cell::new(0u32));

        let c = Rc::clone(&counter);
        block_on(async move {
            for _ in 0..5 {
                let c_inner = Rc::clone(&c);
                spawn(async move {
                    c_inner.set(c_inner.get() + 1);
                });
            }
            // Yield to let spawned tasks run
            yield_now().await;
        });

        assert_eq!(counter.get(), 5);
    }

    #[test]
    fn block_on_empty_future_completes() {
        block_on(async {});
    }
}
