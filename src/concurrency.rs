//! 自适应并发控制：根据限流/过载反馈动态调整章节 worker 数。

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tracing::{info, warn};

struct State {
    limit: usize,
    active: usize,
    successes: u32,
}

pub struct AdaptiveSemaphore {
    minimum: usize,
    maximum: usize,
    adaptive: bool,
    recover_after: u32,
    state: Mutex<State>,
    notify: Notify,
}

/// 持有一个并发槽位；drop 时自动归还并唤醒等待者。
pub struct AdaptivePermit {
    sem: Arc<AdaptiveSemaphore>,
}

impl AdaptiveSemaphore {
    pub fn new(
        initial: usize,
        minimum: usize,
        maximum: usize,
        adaptive: bool,
        recover_after: u32,
    ) -> Arc<Self> {
        let minimum = minimum.max(1);
        let maximum = maximum.max(minimum);
        let initial = initial.clamp(minimum, maximum);
        Arc::new(Self {
            minimum,
            maximum,
            adaptive,
            recover_after: recover_after.max(1),
            state: Mutex::new(State {
                limit: initial,
                active: 0,
                successes: 0,
            }),
            notify: Notify::new(),
        })
    }

    pub async fn acquire(self: &Arc<Self>) -> AdaptivePermit {
        loop {
            // 先登记等待，避免 release/upscale 恰好发生在检查与 await 之间
            let notified = self.notify.notified();
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.active < state.limit {
                    state.active += 1;
                    return AdaptivePermit {
                        sem: Arc::clone(self),
                    };
                }
            }
            notified.await;
        }
    }

    pub fn limit(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .limit
    }

    pub fn active(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active
    }

    pub fn is_adaptive(&self) -> bool {
        self.adaptive
    }

    pub fn on_rate_limit(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.successes = 0;
        if !self.adaptive {
            return;
        }
        if state.limit > self.minimum {
            state.limit -= 1;
            let limit = state.limit;
            drop(state);
            warn!("自适应并发下调至 {limit}（触发限流/过载）");
        }
    }

    pub fn on_success(&self) {
        if !self.adaptive {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.successes += 1;
        if state.successes >= self.recover_after && state.limit < self.maximum {
            state.limit += 1;
            state.successes = 0;
            let limit = state.limit;
            drop(state);
            info!("自适应并发上调至 {limit}");
            self.notify.notify_one();
        }
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active = state.active.saturating_sub(1);
        drop(state);
        self.notify.notify_one();
    }
}

impl Drop for AdaptivePermit {
    fn drop(&mut self) {
        self.sem.release();
    }
}
