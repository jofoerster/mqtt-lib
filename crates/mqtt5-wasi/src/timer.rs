use std::time::Duration;

/// Non-blocking sleep that yields to the executor.
///
/// Uses `Instant::now()` + yielding instead of `std::thread::sleep`,
/// because `thread::sleep` blocks the entire WASI component.
pub async fn sleep(duration: Duration) {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        crate::executor::yield_now().await;
    }
}
