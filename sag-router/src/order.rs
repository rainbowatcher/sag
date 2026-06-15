use std::collections::HashMap;
use std::sync::Arc;

use sag_config::{RouteTarget, Strategy};
use tokio::sync::RwLock;

/// Order the target list according to the route's strategy.
///
/// `FillFirst` groups targets by priority (descending) and shuffles within
/// each group. `RoundRobin` rotates the list so each request starts at a
/// different position, persisting the per-model counter across calls.
pub async fn order_targets(
    targets: Vec<RouteTarget>,
    model_name: &str,
    strategy: Strategy,
    counters: &Arc<RwLock<HashMap<String, u64>>>,
) -> Vec<RouteTarget> {
    match strategy {
        Strategy::FillFirst => order_fill_first(targets),
        Strategy::RoundRobin => order_round_robin(targets, model_name, counters).await,
    }
}

/// Group by priority (descending), shuffle within each priority group.
fn order_fill_first(mut targets: Vec<RouteTarget>) -> Vec<RouteTarget> {
    // Sort by priority descending so same-priority items are adjacent.
    targets.sort_by_key(|t| std::cmp::Reverse(t.priority));

    // Shuffle within each priority group.
    let mut i = 0;
    while i < targets.len() {
        let prio = targets[i].priority;
        let mut j = i + 1;
        while j < targets.len() && targets[j].priority == prio {
            j += 1;
        }
        fastrand::shuffle(&mut targets[i..j]);
        i = j;
    }
    targets
}

/// Rotate the list so each request starts at a different position,
/// using a shared counter keyed by model name.
async fn order_round_robin(
    targets: Vec<RouteTarget>,
    model_name: &str,
    counters: &Arc<RwLock<HashMap<String, u64>>>,
) -> Vec<RouteTarget> {
    let mut guard = counters.write().await;
    let counter = guard.entry(model_name.to_string()).or_insert(0);
    let len = targets.len();
    let start = (*counter % len as u64) as usize;
    *counter += 1;
    let mut rotated = targets;
    rotated.rotate_left(start);
    rotated
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_config::RouteTarget;
    use std::collections::HashMap;

    fn target(provider: &str, priority: u32) -> RouteTarget {
        RouteTarget {
            provider: provider.into(),
            real_model: None,
            priority,
            retries: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn fill_first_groups_by_priority_descending() {
        let targets = vec![
            target("low", 1),
            target("high-a", 10),
            target("high-b", 10),
            target("mid", 5),
        ];
        let ordered = order_fill_first(targets);

        // Priorities should be 10, 10, 5, 1
        let priorities: Vec<u32> = ordered.iter().map(|t| t.priority).collect();
        assert_eq!(priorities, vec![10, 10, 5, 1]);

        // Providers within the priority=10 group should be the same set
        let high_group: Vec<&str> = ordered
            .iter()
            .take(2)
            .map(|t| t.provider.as_str())
            .collect();
        assert!(high_group.contains(&"high-a") && high_group.contains(&"high-b"));
    }

    #[tokio::test]
    async fn round_robin_rotates_by_counter() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];

        // First call: starts at 0
        let r1 = order_targets(targets.clone(), "model", Strategy::RoundRobin, &counters).await;
        assert_eq!(r1[0].provider, "a");

        // Second call: starts at 1
        let r2 = order_targets(targets.clone(), "model", Strategy::RoundRobin, &counters).await;
        assert_eq!(r2[0].provider, "b");

        // Third call: starts at 2
        let r3 = order_targets(targets.clone(), "model", Strategy::RoundRobin, &counters).await;
        assert_eq!(r3[0].provider, "c");

        // Fourth call: wraps back to 0
        let r4 = order_targets(targets, "model", Strategy::RoundRobin, &counters).await;
        assert_eq!(r4[0].provider, "a");
    }

    #[tokio::test]
    async fn round_robin_keys_are_per_model() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let targets = vec![target("a", 0), target("b", 0)];

        let r1 = order_targets(targets.clone(), "model-a", Strategy::RoundRobin, &counters).await;
        let r2 = order_targets(targets.clone(), "model-b", Strategy::RoundRobin, &counters).await;

        // Each model has its own counter, so both should start at 0
        assert_eq!(r1[0].provider, "a");
        assert_eq!(r2[0].provider, "a");
    }
}
