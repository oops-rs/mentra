//! The ring's memory: which member is answering, how many times in a row it
//! has failed, and which members were left behind and when.
//!
//! Every transition here is a pure function from one [`RingHealth`] to the
//! next. The provider holds the current value behind a mutex and swaps it for
//! the returned one, so the whole failover policy is testable with nothing but
//! an [`Instant`] — no clock injection, no fake provider, no runtime.
//!
//! # The rules
//!
//! - **One member answers at a time**: the *current* one. The ring starts on
//!   the first member, the preferred one.
//! - **Consecutive failures rotate.** A counted failure on the current member
//!   increments its streak; reaching
//!   [`failure_threshold`](GatewayRingPolicy::failure_threshold) rotates the
//!   ring to the next member. A success resets the streak.
//! - **A rejection rotates at once.** A member that answers `4xx` — the wrong
//!   key, a path it does not serve, a request shape it refuses — is not going
//!   to improve by being asked again, so it costs one failure, not five.
//! - **A member that was left is on probation when the ring comes back to
//!   it.** Its first failure rotates again immediately; its first success
//!   clears the probation. This is what keeps a dead ring cycling quickly
//!   instead of spending a full streak on each corpse.
//! - **Coming back is the policy's call.** With a
//!   [`cooldown`](GatewayRingPolicy::cooldown), the ring drifts back to a more
//!   preferred member once that member has rested that long; without one, the
//!   ring is sticky and only reaches a member again by rotating around to it.

use std::time::{Duration, Instant};

/// How a ring decides when to leave a member and when to come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatewayRingPolicy {
    /// Consecutive counted failures on the active member before the ring
    /// rotates away from it.
    ///
    /// Counted across calls, not within one: a member that fails, is retried
    /// by the runtime's own backoff, and fails again has a streak of two.
    /// For the rotation to happen inside a single turn this should be no
    /// larger than the runtime's retry budget; a larger threshold still
    /// rotates, just across turns. `0` is read as `1`.
    pub failure_threshold: u32,
    /// How long a member the ring rotated away from rests before a *more
    /// preferred* one is tried again.
    ///
    /// `Some` makes the ring drift back: once the preferred member has rested
    /// this long, the next call probes it, and one failure sends the ring back
    /// to where it was. `None` makes the ring sticky: it stays on whichever
    /// member last worked, and only reaches an earlier one by rotating all the
    /// way around.
    pub cooldown: Option<Duration>,
}

impl GatewayRingPolicy {
    /// Five consecutive failures, then rotate; drift back to the preferred
    /// member after it has rested a minute.
    pub const DEFAULT_FAILURE_THRESHOLD: u32 = 5;
    /// See [`Self::DEFAULT_FAILURE_THRESHOLD`].
    pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

    /// A sticky ring: rotate on the default threshold and never drift back.
    #[must_use]
    pub const fn sticky() -> Self {
        Self {
            failure_threshold: Self::DEFAULT_FAILURE_THRESHOLD,
            cooldown: None,
        }
    }

    /// The same policy with a different failure threshold.
    #[must_use]
    pub const fn with_failure_threshold(self, failure_threshold: u32) -> Self {
        Self {
            failure_threshold,
            ..self
        }
    }

    /// The same policy with a different cooldown, or none.
    #[must_use]
    pub const fn with_cooldown(self, cooldown: Option<Duration>) -> Self {
        Self { cooldown, ..self }
    }

    const fn threshold(self) -> u32 {
        if self.failure_threshold == 0 {
            1
        } else {
            self.failure_threshold
        }
    }
}

impl Default for GatewayRingPolicy {
    fn default() -> Self {
        Self {
            failure_threshold: Self::DEFAULT_FAILURE_THRESHOLD,
            cooldown: Some(Self::DEFAULT_COOLDOWN),
        }
    }
}

/// What a failed call says about the member that failed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The member's fault, or nobody can tell: a transport error, a `5xx`, a
    /// rate limit, a stream that broke. One more on the streak.
    Counted,
    /// The member rejected the request outright — a `4xx` other than a rate
    /// limit or a timeout. Waiting will not change its answer, so the ring
    /// leaves at once.
    Immediate,
    /// The request's own fault: too long for the model, malformed, or asking
    /// for a capability the wire lacks. Every member would say the same, so
    /// the ring neither counts it nor moves.
    NotTheMembersFault,
}

/// One member's standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct MemberHealth {
    /// Consecutive counted failures since the last success.
    streak: u32,
    /// Whether the ring has come back to this member after leaving it and
    /// has not yet seen it succeed.
    probation: bool,
    /// When the ring rotated away from this member, if it ever did and has
    /// not since seen it succeed.
    left_at: Option<Instant>,
}

impl MemberHealth {
    fn succeeded(self) -> Self {
        Self::default()
    }

    fn failed(self) -> Self {
        Self {
            streak: self.streak.saturating_add(1),
            ..self
        }
    }

    fn left(self, now: Instant) -> Self {
        Self {
            streak: 0,
            probation: false,
            left_at: Some(now),
        }
    }

    /// Selected again after having been left.
    fn returned_to(self) -> Self {
        Self {
            streak: 0,
            probation: self.left_at.is_some(),
            left_at: None,
        }
    }

    fn rested_for(self, cooldown: Duration, now: Instant) -> bool {
        self.left_at
            .is_some_and(|left_at| now.saturating_duration_since(left_at) >= cooldown)
    }
}

/// Which member a call should go to, and whether the ring just came back to
/// it after having left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Selection {
    pub(super) index: usize,
    pub(super) returned: bool,
}

/// The ring rotated from one member to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rotation {
    pub(super) from: usize,
    pub(super) to: usize,
    /// The streak that caused it — `0` for an immediate rotation.
    pub(super) after_failures: u32,
}

/// The ring's whole state. Values are replaced, never mutated in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RingHealth {
    current: usize,
    members: Vec<MemberHealth>,
}

impl RingHealth {
    pub(super) fn new(members: usize) -> Self {
        Self {
            current: 0,
            members: vec![MemberHealth::default(); members],
        }
    }

    pub(super) fn current(&self) -> usize {
        self.current
    }

    /// Whether the current member is on probation.
    pub(super) fn on_probation(&self) -> bool {
        self.members[self.current].probation
    }

    /// The member's consecutive counted failures since its last success.
    pub(super) fn streak_of(&self, index: usize) -> u32 {
        self.members[index].streak
    }

    /// Chooses the member for the next call.
    ///
    /// Under a cooldown, a more preferred member that has rested long enough
    /// wins over the current one. Otherwise the current member stays.
    pub(super) fn select(&self, policy: GatewayRingPolicy, now: Instant) -> (Self, Selection) {
        let rested = policy.cooldown.and_then(|cooldown| {
            (0..self.current).find(|&index| self.members[index].rested_for(cooldown, now))
        });
        match rested {
            Some(index) => (
                self.with_current(index),
                Selection {
                    index,
                    returned: true,
                },
            ),
            None => (
                self.clone(),
                Selection {
                    index: self.current,
                    returned: false,
                },
            ),
        }
    }

    /// The member answered.
    pub(super) fn succeeded(&self, index: usize) -> Self {
        self.with_member(index, self.members[index].succeeded())
    }

    /// The member failed. Returns the rotation this caused, if any.
    ///
    /// A failure on a member that is no longer current — another call already
    /// rotated away from it — changes nothing, so two concurrent callers
    /// cannot rotate the ring twice for one outage.
    pub(super) fn failed(
        &self,
        index: usize,
        kind: FailureKind,
        policy: GatewayRingPolicy,
        now: Instant,
    ) -> (Self, Option<Rotation>) {
        if index != self.current || kind == FailureKind::NotTheMembersFault {
            return (self.clone(), None);
        }
        let member = self.members[index].failed();
        let rotate = match kind {
            FailureKind::Immediate => true,
            FailureKind::Counted => member.probation || member.streak >= policy.threshold(),
            FailureKind::NotTheMembersFault => false,
        };
        if !rotate {
            return (self.with_member(index, member), None);
        }
        let after_failures = match kind {
            FailureKind::Immediate => 0,
            _ => member.streak,
        };
        let next = self.next_after(index, policy, now);
        let rotated = self.with_member(index, member.left(now)).with_current(next);
        (
            rotated,
            Some(Rotation {
                from: index,
                to: next,
                after_failures,
            }),
        )
    }

    /// The next member in ring order, skipping any that was left and has not
    /// yet rested for the cooldown — unless every other member is in that
    /// state, in which case the next one is taken anyway and probed.
    fn next_after(&self, index: usize, policy: GatewayRingPolicy, now: Instant) -> usize {
        let count = self.members.len();
        let candidates = (1..count).map(|step| (index + step) % count);
        match policy.cooldown {
            Some(cooldown) => candidates
                .clone()
                .find(|&candidate| {
                    let member = self.members[candidate];
                    member.left_at.is_none() || member.rested_for(cooldown, now)
                })
                .or_else(|| candidates.clone().next())
                .unwrap_or(index),
            None => candidates.clone().next().unwrap_or(index),
        }
    }

    fn with_current(&self, index: usize) -> Self {
        Self {
            current: index,
            members: self
                .members
                .iter()
                .enumerate()
                .map(|(i, member)| {
                    if i == index {
                        member.returned_to()
                    } else {
                        *member
                    }
                })
                .collect(),
        }
    }

    fn with_member(&self, index: usize, health: MemberHealth) -> Self {
        Self {
            current: self.current,
            members: self
                .members
                .iter()
                .enumerate()
                .map(|(i, member)| if i == index { health } else { *member })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn policy(threshold: u32, cooldown: Option<Duration>) -> GatewayRingPolicy {
        GatewayRingPolicy {
            failure_threshold: threshold,
            cooldown,
        }
    }

    fn fail(
        health: &RingHealth,
        index: usize,
        policy: GatewayRingPolicy,
        now: Instant,
    ) -> (RingHealth, Option<Rotation>) {
        health.failed(index, FailureKind::Counted, policy, now)
    }

    #[test]
    fn the_ring_starts_on_the_preferred_member() {
        let health = RingHealth::new(3);
        let (_, selection) = health.select(GatewayRingPolicy::default(), Instant::now());

        assert_eq!(
            selection,
            Selection {
                index: 0,
                returned: false
            }
        );
    }

    #[test]
    fn failures_short_of_the_threshold_stay_put() {
        let policy = policy(3, None);
        let now = Instant::now();
        let health = RingHealth::new(2);

        let (health, rotation) = fail(&health, 0, policy, now);
        assert_eq!(rotation, None);
        let (health, rotation) = fail(&health, 0, policy, now);
        assert_eq!(rotation, None);
        assert_eq!(health.current(), 0);
    }

    #[test]
    fn the_threshold_rotates_to_the_next_member() {
        let policy = policy(3, None);
        let now = Instant::now();
        let health = RingHealth::new(2);

        let (health, _) = fail(&health, 0, policy, now);
        let (health, _) = fail(&health, 0, policy, now);
        let (health, rotation) = fail(&health, 0, policy, now);

        assert_eq!(
            rotation,
            Some(Rotation {
                from: 0,
                to: 1,
                after_failures: 3
            })
        );
        assert_eq!(health.current(), 1);
    }

    #[test]
    fn a_success_resets_the_streak() {
        let policy = policy(2, None);
        let now = Instant::now();
        let health = RingHealth::new(2);

        let (health, _) = fail(&health, 0, policy, now);
        let health = health.succeeded(0);
        let (health, rotation) = fail(&health, 0, policy, now);

        assert_eq!(rotation, None);
        assert_eq!(health.current(), 0);
    }

    #[test]
    fn a_rejection_rotates_at_once() {
        let policy = policy(5, None);
        let now = Instant::now();
        let health = RingHealth::new(2);

        let (health, rotation) = health.failed(0, FailureKind::Immediate, policy, now);

        assert_eq!(
            rotation,
            Some(Rotation {
                from: 0,
                to: 1,
                after_failures: 0
            })
        );
        assert_eq!(health.current(), 1);
    }

    #[test]
    fn the_requests_own_fault_changes_nothing() {
        let policy = policy(1, None);
        let now = Instant::now();
        let health = RingHealth::new(2);

        let (after, rotation) = health.failed(0, FailureKind::NotTheMembersFault, policy, now);

        assert_eq!(rotation, None);
        assert_eq!(after, health);
    }

    #[test]
    fn a_failure_on_a_member_no_longer_current_is_stale() {
        let policy = policy(1, None);
        let now = Instant::now();
        let health = RingHealth::new(3);

        let (health, _) = fail(&health, 0, policy, now);
        assert_eq!(health.current(), 1);
        // A concurrent caller that was still on member 0 reports in late.
        let (after, rotation) = fail(&health, 0, policy, now);

        assert_eq!(rotation, None);
        assert_eq!(after, health);
    }

    #[test]
    fn a_threshold_of_zero_reads_as_one() {
        let policy = policy(0, None);
        let (health, rotation) = fail(&RingHealth::new(2), 0, policy, Instant::now());

        assert!(rotation.is_some());
        assert_eq!(health.current(), 1);
    }

    #[test]
    fn a_sticky_ring_never_drifts_back() {
        let policy = policy(1, None);
        let now = Instant::now();
        let (health, _) = fail(&RingHealth::new(2), 0, policy, now);
        let health = health.succeeded(1);

        let (_, selection) = health.select(policy, now + Duration::from_secs(3600));

        assert_eq!(
            selection,
            Selection {
                index: 1,
                returned: false
            }
        );
    }

    #[test]
    fn a_sticky_ring_wraps_around_and_probes_the_member_it_left() {
        let policy = policy(2, None);
        let now = Instant::now();
        let health = RingHealth::new(2);

        let (health, _) = fail(&health, 0, policy, now);
        let (health, _) = fail(&health, 0, policy, now);
        let (health, _) = fail(&health, 1, policy, now);
        let (health, rotation) = fail(&health, 1, policy, now);
        assert_eq!(rotation.map(|rotation| rotation.to), Some(0));
        assert!(health.on_probation());

        // Back on the member it left: one failure is enough to move on again.
        let (health, rotation) = fail(&health, 0, policy, now);
        assert_eq!(rotation.map(|rotation| rotation.to), Some(1));
        assert!(health.on_probation());

        // And one success settles it.
        let health = health.succeeded(1);
        assert!(!health.on_probation());
        let (_, rotation) = fail(&health, 1, policy, now);
        assert_eq!(rotation, None);
    }

    #[test]
    fn a_cooldown_brings_the_ring_back_to_the_preferred_member() {
        let policy = policy(1, Some(SECOND));
        let now = Instant::now();
        let (health, _) = fail(&RingHealth::new(2), 0, policy, now);
        let health = health.succeeded(1);

        let (unchanged, selection) = health.select(policy, now + SECOND / 2);
        assert_eq!(selection.index, 1);
        assert_eq!(unchanged, health);

        let (returned, selection) = health.select(policy, now + SECOND);
        assert_eq!(
            selection,
            Selection {
                index: 0,
                returned: true
            }
        );
        assert_eq!(returned.current(), 0);
        assert!(returned.on_probation());
    }

    #[test]
    fn a_failed_probe_rotates_at_once_and_rests_again() {
        let policy = policy(5, Some(SECOND));
        let start = Instant::now();
        let mut health = RingHealth::new(2);
        for _ in 0..5 {
            health = fail(&health, 0, policy, start).0;
        }
        assert_eq!(health.current(), 1);

        let rested = start + SECOND;
        let (health, selection) = health.select(policy, rested);
        assert!(selection.returned);
        let (health, rotation) = fail(&health, 0, policy, rested);
        assert_eq!(
            rotation,
            Some(Rotation {
                from: 0,
                to: 1,
                after_failures: 1
            })
        );

        // The rest is measured from the failed probe, not the first departure.
        let (_, selection) = health.select(policy, rested + SECOND / 2);
        assert_eq!(selection.index, 1);
        let (_, selection) = health.select(policy, rested + SECOND);
        assert_eq!(selection.index, 0);
    }

    #[test]
    fn a_successful_probe_clears_the_probation() {
        let policy = policy(2, Some(SECOND));
        let now = Instant::now();
        let (health, _) = fail(&RingHealth::new(2), 0, policy, now);
        let (health, _) = fail(&health, 0, policy, now);
        let (health, _) = health.select(policy, now + SECOND);
        assert!(health.on_probation());
        let health = health.succeeded(0);

        // Settled again: one failure is short of the threshold, as it was
        // before the member was ever left.
        assert!(!health.on_probation());
        let (_, rotation) = fail(&health, 0, policy, now + SECOND);
        assert_eq!(rotation, None);
    }

    #[test]
    fn rotation_skips_members_still_resting_under_a_cooldown() {
        let policy = policy(1, Some(SECOND));
        let now = Instant::now();
        let health = RingHealth::new(3);

        let (health, _) = fail(&health, 0, policy, now);
        let (health, _) = fail(&health, 1, policy, now);
        assert_eq!(health.current(), 2);
        // 2 dies before anyone has rested: the ring takes the next anyway.
        let (health, rotation) = fail(&health, 2, policy, now);
        assert_eq!(rotation.map(|rotation| rotation.to), Some(0));
        assert!(health.on_probation());
        // 0 fails its probe; 1 has not rested either, but 2 was just left too,
        // so the next in ring order is taken.
        let (health, rotation) = fail(&health, 0, policy, now + SECOND / 2);
        assert_eq!(rotation.map(|rotation| rotation.to), Some(1));
        // Once 2 has rested and 1 fails, the ring prefers the rested member
        // over the one that was left most recently.
        let later = now + SECOND;
        let (_, rotation) = fail(&health, 1, policy, later);
        assert_eq!(rotation.map(|rotation| rotation.to), Some(2));
    }

    #[test]
    fn a_ring_of_one_rotates_onto_itself_and_stays_on_probation() {
        let policy = policy(1, None);
        let now = Instant::now();
        let (health, rotation) = fail(&RingHealth::new(1), 0, policy, now);

        assert_eq!(
            rotation,
            Some(Rotation {
                from: 0,
                to: 0,
                after_failures: 1
            })
        );
        assert_eq!(health.current(), 0);
        assert!(health.on_probation());
    }
}
