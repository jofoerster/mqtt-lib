use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

type BoxFuture = Pin<Box<dyn Future<Output = ()>>>;

thread_local! {
    static SPAWN_QUEUE: RefCell<VecDeque<BoxFuture>> = RefCell::new(VecDeque::new());
}

/// Spawn a future onto the executor.
///
/// Must be called from within `block_on`.
pub fn spawn(task: impl Future<Output = ()> + 'static) {
    SPAWN_QUEUE.with(|queue| {
        queue.borrow_mut().push_back(Box::pin(task));
    });
}

/// Yield control back to the executor so other tasks can run.
///
/// Returns a future that completes on the next poll cycle.
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
/// Uses non-blocking I/O with yielding: tasks that encounter `WouldBlock`
/// return `Poll::Pending`, allowing the executor to poll other tasks.
/// A brief sleep prevents busy-spinning when all tasks are idle.
pub fn block_on(main: impl Future<Output = ()> + 'static) {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    let mut tasks: Vec<BoxFuture> = vec![Box::pin(main)];
    let mut cycle: u64 = 0;

    loop {
        // Drain newly spawned tasks
        let prev_len = tasks.len();
        SPAWN_QUEUE.with(|queue| {
            let mut q = queue.borrow_mut();
            while let Some(task) = q.pop_front() {
                tasks.push(task);
            }
        });
        if tasks.len() > prev_len {
            eprintln!(
                "[executor] cycle {cycle}: +{} tasks (total {})",
                tasks.len() - prev_len,
                tasks.len()
            );
        }

        if tasks.is_empty() {
            break;
        }

        let mut made_progress = false;

        // Poll all tasks, removing completed ones
        tasks.retain_mut(|task| match task.as_mut().poll(&mut cx) {
            Poll::Ready(()) => {
                made_progress = true;
                false // remove completed task
            }
            Poll::Pending => true, // keep pending task
        });

        cycle += 1;

        if !made_progress {
            // All tasks are waiting for I/O. Use a WASI-safe sleep
            // that doesn't block the component entirely.
            #[cfg(target_os = "wasi")]
            {
                // Use wasi:clocks pollable for a non-blocking-style wait
                let pollable =
                    wasi::clocks::monotonic_clock::subscribe_duration(1_000_000); // 1ms in ns
                pollable.block();
            }
            #[cfg(not(target_os = "wasi"))]
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
}

fn noop_waker() -> Waker {
    fn no_op(_: *const ()) {}
    fn clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &VTABLE)
    }
    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}
