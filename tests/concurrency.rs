use std::time::Duration;

use bookcraft::concurrency::AdaptiveSemaphore;

#[tokio::test]
async fn waits_for_release() {
    let sem = AdaptiveSemaphore::new(1, 1, 3, false, 30);
    let permit = sem.acquire().await;
    let sem2 = sem.clone();
    let waiter = tokio::spawn(async move {
        let _permit = sem2.acquire().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished());
    drop(permit);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("waiter should wake")
        .expect("waiter task");
}

#[tokio::test]
async fn adapts_down_and_up() {
    let sem = AdaptiveSemaphore::new(3, 1, 5, true, 2);
    sem.on_rate_limit();
    assert_eq!(sem.limit(), 2);
    sem.on_success();
    assert_eq!(sem.limit(), 2);
    sem.on_success();
    assert_eq!(sem.limit(), 3);
    sem.on_rate_limit();
    sem.on_rate_limit();
    assert_eq!(sem.limit(), 1);
    sem.on_rate_limit();
    assert_eq!(sem.limit(), 1);
}

#[tokio::test]
async fn fixed_mode_ignores_feedback() {
    let sem = AdaptiveSemaphore::new(2, 1, 5, false, 1);
    sem.on_rate_limit();
    sem.on_success();
    assert_eq!(sem.limit(), 2);
}
