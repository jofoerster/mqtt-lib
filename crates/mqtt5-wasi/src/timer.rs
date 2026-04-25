/// Non-blocking sleep that yields to the executor.
///
/// Uses `Instant::now()` + yielding instead of `std::thread::sleep`,
/// because `thread::sleep` blocks the entire WASI component.
pub async fn sleep(duration: std::time::Duration) {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        crate::executor::yield_now().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_completes_after_duration() {
        let start = std::time::Instant::now();
        let duration = std::time::Duration::from_millis(50);

        crate::executor::block_on(async move {
            sleep(duration).await;
        });

        assert!(start.elapsed() >= duration);
    }

    #[test]
    fn sleep_zero_completes_immediately() {
        let start = std::time::Instant::now();

        crate::executor::block_on(async {
            sleep(std::time::Duration::ZERO).await;
        });

        assert!(start.elapsed() < std::time::Duration::from_millis(50));
    }
}
