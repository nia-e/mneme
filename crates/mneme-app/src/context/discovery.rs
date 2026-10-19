//! Shared bounds and fair ordering for one frozen context discovery window.
//! These mechanics do not retrieve, repack, or create new discovery anchors.

use std::time::{Duration, Instant};

pub(super) const MAX_LINKED_CAPACITY: usize = 256;
pub(super) const LINKED_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Keep two initial discovery lanes fair without assigning relevance to scene
/// counts. This is also the presentation order after same-edition origin union.
pub(super) fn interleave<T>(left: Vec<T>, right: Vec<T>) -> Vec<T> {
    let mut left = left.into_iter();
    let mut right = right.into_iter();
    let mut result = Vec::new();
    loop {
        let a = left.next();
        let b = right.next();
        if a.is_none() && b.is_none() {
            break;
        }
        result.extend(a);
        result.extend(b);
    }
    result
}

pub(super) fn remaining(deadline: Instant) -> Option<Duration> {
    let duration = deadline.saturating_duration_since(Instant::now());
    (!duration.is_zero()).then_some(duration)
}
