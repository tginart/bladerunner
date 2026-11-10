use super::super::PersistenceMsg;
use super::super::support::create_test_actor;
use super::*;

fn rate_limited(retry_after_secs: Option<u64>) -> SamplingErrorInfo {
    SamplingErrorInfo {
        kind: SamplingErrorKind::RateLimited,
        message: "Too Many Requests (429)".to_string(),
        status_code: Some(429),
        is_retryable: true,
        retry_after_secs,
        should_retry: None,
        error_code: None,
        model_metadata: None,
        empty_response_context: None,
        doom_loop_triggers: None,
        doom_loop_aborted_at_chunk: None,
        credential: xai_grok_sampling_types::SentCredential::Unknown,
    }
}

async fn actor_with_sampling(
    sampling: Option<SubagentSampling>,
) -> (
    Arc<SessionActor>,
    tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) {
    let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = create_test_actor(50_000, 100_000, 85, gateway_tx, persistence_tx).await;
    actor.tool_context.subagent_sampling = sampling;
    (Arc::new(actor), persistence_rx)
}

fn child_sampling() -> SubagentSampling {
    SubagentSampling::new(Arc::new(Semaphore::new(4)))
}

#[test]
fn budget_waits_until_attempts_are_spent() {
    let mut budget = RateLimitWaitBudget::default();
    for attempt in 1..=MAX_WAITS_PER_TURN {
        assert_eq!(
            budget.decide(0, |_| Duration::from_secs(1)),
            RateLimitWaitDecision::Wait {
                attempt,
                backoff: Duration::from_secs(1),
            }
        );
    }
    assert_eq!(
        budget.decide(0, |_| Duration::from_secs(1)),
        RateLimitWaitDecision::AttemptsSpent {
            attempts: MAX_WAITS_PER_TURN,
        }
    );
}

#[test]
fn budget_stops_before_a_wait_that_overruns_the_total() {
    let mut budget = RateLimitWaitBudget::default();
    let long = MAX_TOTAL_WAIT_PER_TURN / 2;
    assert!(matches!(
        budget.decide(0, |_| long),
        RateLimitWaitDecision::Wait { attempt: 1, .. }
    ));
    assert!(matches!(
        budget.decide(0, |_| long),
        RateLimitWaitDecision::Wait { attempt: 2, .. }
    ));
    assert_eq!(
        budget.decide(0, |_| Duration::from_millis(1)),
        RateLimitWaitDecision::TotalWaitSpent { attempts: 2 }
    );
}

#[test]
fn budget_resets_for_a_new_turn() {
    let mut budget = RateLimitWaitBudget::default();
    for _ in 0..MAX_WAITS_PER_TURN {
        budget.decide(3, |_| Duration::from_secs(1));
    }
    assert!(matches!(
        budget.decide(4, |_| Duration::from_secs(1)),
        RateLimitWaitDecision::Wait { attempt: 1, .. }
    ));
}

/// The sampler backoff schedule must fit the total budget closely, so the
/// attempt cap and the time cap run out at about the same point.
#[test]
fn default_backoffs_use_most_of_the_total_budget() {
    let unjittered: Duration = (1..=MAX_WAITS_PER_TURN)
        .map(|attempt| {
            let base_s = 2u64 << (attempt - 1);
            Duration::from_secs(base_s.min(30))
        })
        .sum();
    assert_eq!(unjittered, MAX_TOTAL_WAIT_PER_TURN);
}

#[test]
fn backoff_honors_retry_after_and_caps_it() {
    assert_eq!(rate_limit_backoff(1, Some(7)), Duration::from_secs(7));
    assert_eq!(
        rate_limit_backoff(1, Some(3_600)),
        xai_grok_sampler::retry::MAX_RETRY_BACKOFF
    );
    let fallback = rate_limit_backoff(1, Some(0));
    assert!(fallback > Duration::ZERO && fallback <= Duration::from_secs(3));
}

#[test]
fn sampling_limit_defaults_to_max_concurrent_and_is_clamped() {
    assert_eq!(resolve_sampling_limit(None, 32), 32);
    assert_eq!(resolve_sampling_limit(Some("4"), 32), 4);
    assert_eq!(resolve_sampling_limit(Some("0"), 32), 32);
    assert_eq!(resolve_sampling_limit(Some("lots"), 32), 32);
    assert_eq!(
        resolve_sampling_limit(Some("100000"), 32),
        MAX_SAMPLING_LIMIT
    );
    assert_eq!(resolve_sampling_limit(None, 0), 1);
}

/// A subagent turn that gets a 429 waits and asks for a resubmit instead of
/// failing. Before this, `handle_sampling_failure` failed the child at once.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn subagent_waits_on_429_and_resubmits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = actor_with_sampling(Some(child_sampling())).await;
            let started = tokio::time::Instant::now();
            let recovery = actor
                .handle_sampling_failure(rate_limited(Some(5)))
                .await
                .expect("a child must wait on a 429, not fail");
            assert!(matches!(
                recovery,
                SamplerFailureRecovery::CompactAndResubmit
            ));
            assert!(started.elapsed() >= Duration::from_secs(5));
        })
        .await;
}

/// Once the turn's budget is spent, the 429 fails the child as before.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn subagent_fails_on_429_after_the_budget_is_spent() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = actor_with_sampling(Some(child_sampling())).await;
            for _ in 0..MAX_WAITS_PER_TURN {
                assert!(
                    actor
                        .handle_sampling_failure(rate_limited(Some(1)))
                        .await
                        .is_ok()
                );
            }
            let error = actor
                .handle_sampling_failure(rate_limited(Some(1)))
                .await
                .err()
                .expect("a spent budget must fail the turn");
            assert_eq!(
                error.code,
                crate::sampling::error::RATE_LIMITED_ERROR_CODE.into()
            );
        })
        .await;
}

/// A main session has a user who can retry, so its 429 still fails at once.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn main_session_429_still_fails_at_once() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = actor_with_sampling(None).await;
            let started = tokio::time::Instant::now();
            assert!(
                actor
                    .handle_sampling_failure(rate_limited(Some(5)))
                    .await
                    .is_err()
            );
            assert_eq!(started.elapsed(), Duration::ZERO);
        })
        .await;
}

/// The gate limits concurrent child requests: the permit releases on drop,
/// and a main session takes no permit.
#[tokio::test(flavor = "current_thread")]
async fn sampling_permit_is_gated_for_children_only() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let gate = Arc::new(Semaphore::new(1));
            let (child, _rx) = actor_with_sampling(Some(SubagentSampling::new(gate.clone()))).await;
            let held = child.acquire_subagent_sampling_permit().await;
            assert!(held.is_some());
            assert_eq!(gate.available_permits(), 0);
            drop(held);
            assert_eq!(gate.available_permits(), 1);

            let (main, _rx) = actor_with_sampling(None).await;
            assert!(main.acquire_subagent_sampling_permit().await.is_none());
        })
        .await;
}
