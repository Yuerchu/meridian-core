//! 谁可以现在发一次语音。
//!
//! 放在 `Services` 上，**不能放在 `QqToolExecutor` 里**——那个每一轮重建，
//! 而"每轮最多一次"放在一个每轮重建的东西里根本约束不住任何事。
//!
//! 限的是**发往 Fish 的请求次数**，不是成功发送的次数。只数成功的话，一个反复
//! 失败的模型可以把配额打爆而计数器停在零。

use std::collections::HashMap;
use std::sync::Mutex;

/// 一个会话两次语音之间的最小间隔。语音在群里是打扰，连着发两条比发一条糟得
/// 多，而这个判断模型自己做不了——它不知道自己刚说过。
const COOLDOWN_MS: i64 = 60_000;
/// 同时在合成的上限。Fish 那边按次计费，而这里挡的是"一堆并发请求同时出去"。
const MAX_CONCURRENT: usize = 2;
/// 滚动窗口内的请求上限。cooldown 和并发都只是限速，**不是花费上限**——群里
/// 任何一个人都能触发一次付费合成，所以还要一个总量。
const WINDOW_MS: i64 = 3_600_000;
const MAX_PER_WINDOW: usize = 60;

#[derive(Default)]
struct State {
    /// 会话 -> 上次尝试的时间。
    last_attempt: HashMap<String, i64>,
    /// 本轮已经尝试过的 turn id。
    turns: HashMap<String, i64>,
    /// 滚动窗口内每次尝试的时间戳。
    window: Vec<i64>,
    in_flight: usize,
}

#[derive(Default)]
pub struct VoiceLimiter {
    state: Mutex<State>,
}

/// 一次尝试的许可。**从这里一直持有到消息成功入队**——最终校验到异步入队之间
/// 还有一个窗口，permit 覆盖的就是它。
pub struct SendPermit<'a> {
    limiter: &'a VoiceLimiter,
}

impl Drop for SendPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.limiter.state.lock() {
            state.in_flight = state.in_flight.saturating_sub(1);
        }
    }
}

impl VoiceLimiter {
    /// 要一次尝试的许可。`Err` 里是给模型看的话。
    ///
    /// **计数在这里就发生**，不等结果：这一次要不要花钱，在请求发出去的那一刻
    /// 就定了。
    pub fn try_acquire(&self, session: &str, turn_id: &str, now: i64) -> Result<SendPermit<'_>, String> {
        let mut state = self.state.lock().map_err(|_| "voice limiter poisoned")?;

        state.turns.retain(|_, at| now - *at < WINDOW_MS);
        state.window.retain(|at| now - *at < WINDOW_MS);
        // 冷却过了的条目和不存在的条目答案一样，但只有清掉它，长期运行的进程
        // 才不会为每个见过一面的会话永久留一行。
        state.last_attempt.retain(|_, at| now - *at < COOLDOWN_MS);

        if state.turns.contains_key(turn_id) {
            return Err("a voice message has already been attempted in this turn".into());
        }
        if let Some(last) = state.last_attempt.get(session)
            && now - *last < COOLDOWN_MS
        {
            let wait = (COOLDOWN_MS - (now - *last)) / 1000;
            return Err(format!("too soon — another voice message can be sent in {wait}s"));
        }
        if state.in_flight >= MAX_CONCURRENT {
            return Err("too many voice messages are being synthesised right now".into());
        }
        if state.window.len() >= MAX_PER_WINDOW {
            return Err("the hourly voice budget is used up".into());
        }

        state.turns.insert(turn_id.to_string(), now);
        state.last_attempt.insert(session.to_string(), now);
        state.window.push(now);
        state.in_flight += 1;
        Ok(SendPermit { limiter: self })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_attempt_per_turn() {
        let limiter = VoiceLimiter::default();
        let first = limiter.try_acquire("group:1", "turn-a", 0);
        assert!(first.is_ok());
        drop(first);
        assert!(
            limiter.try_acquire("group:1", "turn-a", 10).is_err(),
            "同一轮里第二次要被拒"
        );
    }

    /// 额度在**尝试**时就占掉，不等成功。只数成功的话，一个反复失败的模型可以
    /// 把 Fish 的配额打爆而计数器停在零。
    #[test]
    fn a_failed_attempt_still_counts() {
        let limiter = VoiceLimiter::default();
        drop(limiter.try_acquire("group:1", "turn-a", 0));
        assert!(
            limiter.try_acquire("group:1", "turn-b", 1_000).is_err(),
            "冷却期内不许再来，哪怕上一次失败了"
        );
    }

    #[test]
    fn a_different_session_has_its_own_cooldown() {
        let limiter = VoiceLimiter::default();
        drop(limiter.try_acquire("group:1", "turn-a", 0));
        assert!(limiter.try_acquire("group:2", "turn-b", 1_000).is_ok());
    }

    #[test]
    fn the_hourly_budget_is_a_ceiling_on_spending() {
        let limiter = VoiceLimiter::default();
        // 每次换一个会话和一个 turn，绕开前两道闸，只剩总量。
        for i in 0..MAX_PER_WINDOW {
            let ok = limiter.try_acquire(&format!("group:{i}"), &format!("t{i}"), i as i64);
            assert!(ok.is_ok(), "第 {i} 次不该被拒");
            drop(ok);
        }
        assert!(limiter.try_acquire("group:new", "t-new", 100).is_err());
    }

    /// 冷却过了的条目不能永久留着：一个长期运行、见过很多会话的进程，
    /// `last_attempt` 会一直涨——每小时的总量挡得住速度，挡不住总数。
    #[test]
    fn expired_cooldowns_do_not_pile_up() {
        let limiter = VoiceLimiter::default();
        for i in 0..10 {
            drop(limiter.try_acquire(&format!("group:{i}"), &format!("t{i}"), 0));
        }
        drop(limiter.try_acquire("group:new", "t-new", COOLDOWN_MS + 1));
        let state = limiter.state.lock().unwrap();
        assert_eq!(state.last_attempt.len(), 1, "只剩下还在冷却里的那一个");
    }

    #[test]
    fn concurrency_is_capped_while_permits_are_held() {
        let limiter = VoiceLimiter::default();
        let a = limiter.try_acquire("group:1", "t1", 0).unwrap();
        let b = limiter.try_acquire("group:2", "t2", 0).unwrap();
        assert!(limiter.try_acquire("group:3", "t3", 0).is_err());
        drop(a);
        drop(b);
        assert!(limiter.try_acquire("group:3", "t3", 0).is_ok());
    }
}
