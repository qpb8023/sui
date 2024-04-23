// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    fmt::{Debug, Formatter},
    sync::Arc,
};

use consensus_config::{AuthorityIndex, Stake};
use parking_lot::RwLock;
use rand::{prelude::SliceRandom, rngs::StdRng, SeedableRng};

use crate::{commit::CommitRange, context::Context, leader_scoring::ReputationScores, Round};

/// The `LeaderSchedule` is responsible for producing the leader schedule across
/// an epoch. The leader schedule is subject to change periodically based on
/// calculated `ReputationScores` of the authorities.
#[derive(Clone)]
pub(crate) struct LeaderSchedule {
    context: Arc<Context>,
    #[allow(unused)]
    num_commits_per_schedule: u64,
    leader_swap_table: Arc<RwLock<LeaderSwapTable>>,
}

#[allow(unused)]
impl LeaderSchedule {
    /// The window where the schedule change takes place in consensus. It represents
    /// number of committed sub dags.
    /// TODO: move this to protocol config
    const CONSENSUS_COMMITS_PER_SCHEDULE: u64 = 300;

    pub(crate) fn new(context: Arc<Context>, leader_swap_table: LeaderSwapTable) -> Self {
        Self {
            context,
            num_commits_per_schedule: Self::CONSENSUS_COMMITS_PER_SCHEDULE,
            leader_swap_table: Arc::new(RwLock::new(leader_swap_table)),
        }
    }

    pub(crate) fn elect_leader(&self, round: u32, leader_offset: u32) -> AuthorityIndex {
        cfg_if::cfg_if! {
            // TODO: we need to differentiate the leader strategy in tests, so for
            // some type of testing (ex sim tests) we can use the staked approach.
            if #[cfg(test)] {
                let leader = AuthorityIndex::new_for_test((round + leader_offset) % self.context.committee.size() as u32);
                let table = self.leader_swap_table.read();
                table.swap(leader, round, leader_offset).unwrap_or(leader)
            } else {
                let leader = self.elect_leader_stake_based(round, leader_offset);
                let table = self.leader_swap_table.read();
                table.swap(leader, round, leader_offset).unwrap_or(leader)
            }
        }
    }

    pub(crate) fn elect_leader_stake_based(&self, round: u32, offset: u32) -> AuthorityIndex {
        assert!((offset as usize) < self.context.committee.size());

        // To ensure that we elect different leaders for the same round (using
        // different offset) we are using the round number as seed to shuffle in
        // a weighted way the results, but skip based on the offset.
        // TODO: use a cache in case this proves to be computationally expensive
        let mut seed_bytes = [0u8; 32];
        seed_bytes[32 - 4..].copy_from_slice(&(round).to_le_bytes());
        let mut rng = StdRng::from_seed(seed_bytes);

        let choices = self
            .context
            .committee
            .authorities()
            .map(|(index, authority)| (index, authority.stake as f32))
            .collect::<Vec<_>>();

        let leader_index = *choices
            .choose_multiple_weighted(&mut rng, self.context.committee.size(), |item| item.1)
            .expect("Weighted choice error: stake values incorrect!")
            .skip(offset as usize)
            .map(|(index, _)| index)
            .next()
            .unwrap();

        leader_index
    }

    /// Atomically updates the `LeaderSwapTable` with the new provided one. Any
    /// leader queried from now on will get calculated according to this swap
    /// table until a new one is provided again.
    fn update_leader_swap_table(&self, table: LeaderSwapTable) {
        let read = self.leader_swap_table.read();
        let old_commit_range = &read.reputation_scores.commit_range;
        let new_commit_range = &table.reputation_scores.commit_range;

        // Unless LeaderSchedule is brand new and using the default commit range
        // of CommitRange(0..0) all future LeaderSwapTables should be calculated
        // from a CommitRange of equal length and immediately following the
        // preceding commit range of the old swap table.
        if *old_commit_range != CommitRange::new(0..0) {
            assert!(
                old_commit_range.is_next_range(new_commit_range),
                "The new LeaderSwapTable has an invalid CommitRange. Old LeaderSwapTable {old_commit_range:?} vs new LeaderSwapTable {new_commit_range:?}",
            );
        }
        drop(read);

        tracing::trace!("Updating {table:?}");

        let mut write = self.leader_swap_table.write();
        *write = table;
    }
}

#[derive(Default, Clone)]
pub(crate) struct LeaderSwapTable {
    /// The list of `f` (by configurable stake) authorities with best scores as
    /// those defined by the provided `ReputationScores`. Those authorities will
    /// be used in the position of the `bad_nodes` on the final leader schedule.
    /// Storing the hostname & stake along side the authority index for debugging.
    pub(crate) good_nodes: Vec<(AuthorityIndex, String, Stake)>,

    /// The set of `f` (by configurable stake) authorities with the worst scores
    /// as those defined by the provided `ReputationScores`. Every time where such
    /// authority is elected as leader on the schedule, it will swapped by one of
    /// the authorities of the `good_nodes`.
    /// Storing the hostname & stake along side the authority index for debugging.
    pub(crate) bad_nodes: BTreeMap<AuthorityIndex, (String, Stake)>,

    // The scores for which the leader swap table was built from. This struct is
    // used for debugging purposes. Once `good_nodes` & `bad_nodes` are identified
    // the `reputation_scores` are no longer needed functionally for the swap table.
    pub(crate) reputation_scores: ReputationScores,
}

#[allow(unused)]
impl LeaderSwapTable {
    // Constructs a new table based on the provided reputation scores. The
    // `swap_stake_threshold` designates the total (by stake) nodes that will be
    // considered as "bad" based on their scores and will be replaced by good nodes.
    // The `swap_stake_threshold` should be in the range of [0 - 33].
    pub(crate) fn new(
        context: Arc<Context>,
        reputation_scores: ReputationScores,
        swap_stake_threshold: u64,
    ) -> Self {
        assert!(
            (0..=33).contains(&swap_stake_threshold),
            "The swap_stake_threshold ({swap_stake_threshold}) should be in range [0 - 33], out of bounds parameter detected"
        );

        // Calculating the good nodes
        let good_nodes = Self::retrieve_first_nodes(
            context.clone(),
            reputation_scores
                .authorities_by_score_desc(context.clone())
                .into_iter(),
            swap_stake_threshold,
        )
        .into_iter()
        .collect::<Vec<(AuthorityIndex, String, Stake)>>();

        // Calculating the bad nodes
        // Reverse the sorted authorities to score ascending so we get the first
        // low scorers up to the provided stake threshold.
        let bad_nodes = Self::retrieve_first_nodes(
            context.clone(),
            reputation_scores
                .authorities_by_score_desc(context.clone())
                .into_iter()
                .rev(),
            swap_stake_threshold,
        )
        .into_iter()
        .map(|(idx, hostname, stake)| (idx, (hostname, stake)))
        .collect::<BTreeMap<AuthorityIndex, (String, Stake)>>();

        good_nodes.iter().for_each(|(idx, hostname, stake)| {
            tracing::debug!(
                "Good node {hostname} with stake {stake} has score {} for {:?}",
                reputation_scores.scores_per_authority[idx.to_owned()],
                reputation_scores.commit_range,
            );
        });

        bad_nodes.iter().for_each(|(idx, (hostname, stake))| {
            tracing::debug!(
                "Bad node {hostname} with stake {stake} has score {} for {:?}",
                reputation_scores.scores_per_authority[idx.to_owned()],
                reputation_scores.commit_range,
            );
        });

        tracing::debug!("Scores used for new LeaderSwapTable: {reputation_scores:?}");

        Self {
            good_nodes,
            bad_nodes,
            reputation_scores,
        }
    }

    /// Checks whether the provided leader is a bad performer and needs to be
    /// swapped in the schedule with a good performer. If not, then the method
    /// returns None. Otherwise the leader to swap with is returned instead. The
    /// `leader_round` & `leader_offset` represents the DAG slot on which the
    /// provided `AuthorityIndex` is a leader on and is used as a seed to random
    /// function in order to calculate the good node that will swap in that round
    /// with the bad node. We are intentionally not doing weighted randomness as
    /// we want to give to all the good nodes equal opportunity to get swapped
    /// with bad nodes and nothave one node with enough stake end up swapping
    /// bad nodes more frequently than the others on the final schedule.
    pub(crate) fn swap(
        &self,
        leader: AuthorityIndex,
        leader_round: Round,
        leader_offset: u32,
    ) -> Option<AuthorityIndex> {
        if self.bad_nodes.contains_key(&leader) {
            // TODO: Re-work swap for the multileader case
            assert!(
                leader_offset == 0,
                "Swap for multi-leader case not implemented yet."
            );
            let mut seed_bytes = [0u8; 32];
            seed_bytes[24..28].copy_from_slice(&leader_round.to_le_bytes());
            seed_bytes[28..32].copy_from_slice(&leader_offset.to_le_bytes());
            let mut rng = StdRng::from_seed(seed_bytes);

            let (idx, _hostname, _stake) = self
                .good_nodes
                .choose(&mut rng)
                .expect("There should be at least one good node available");

            tracing::trace!(
                "Swapping bad leader {} -> {} for round {}",
                leader,
                idx,
                leader_round
            );

            return Some(*idx);
        }
        None
    }

    /// Retrieves the first nodes provided by the iterator `authorities` until the
    /// `stake_threshold` has been reached. The `stake_threshold` should be between
    /// [0, 100] and expresses the percentage of stake that is considered the cutoff.
    /// It's the caller's responsibility to ensure that the elements of the `authorities`
    /// input is already sorted.
    fn retrieve_first_nodes(
        context: Arc<Context>,
        authorities: impl Iterator<Item = (AuthorityIndex, u64)>,
        stake_threshold: u64,
    ) -> Vec<(AuthorityIndex, String, Stake)> {
        let mut filtered_authorities = Vec::new();

        let mut stake = 0;
        for (authority_idx, _score) in authorities {
            stake += context.committee.stake(authority_idx);

            // If the total accumulated stake has surpassed the stake threshold
            // then we omit this last authority and we exit the loop. Important to
            // note that this means if the threshold is too low we may not have
            // any nodes returned.
            if stake > (stake_threshold * context.committee.total_stake()) / 100 as Stake {
                break;
            }

            let authority = context.committee.authority(authority_idx);
            filtered_authorities.push((authority_idx, authority.hostname.clone(), authority.stake));
        }

        filtered_authorities
    }
}

impl Debug for LeaderSwapTable {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!(
            "LeaderSwapTable for {:?}, good_nodes:{:?} with stake:{}, bad_nodes:{:?} with stake:{}",
            self.reputation_scores.commit_range,
            self.good_nodes
                .iter()
                .map(|(idx, _hostname, _stake)| idx.to_owned())
                .collect::<Vec<AuthorityIndex>>(),
            self.good_nodes
                .iter()
                .map(|(_idx, _hostname, stake)| stake)
                .sum::<Stake>(),
            self.bad_nodes.keys().map(|idx| idx.to_owned()),
            self.bad_nodes
                .values()
                .map(|(_hostname, stake)| stake)
                .sum::<Stake>(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::CommitRange;

    #[test]
    fn test_elect_leader() {
        let context = Arc::new(Context::new_for_test(4).0);
        let leader_schedule = LeaderSchedule::new(context, LeaderSwapTable::default());

        assert_eq!(
            leader_schedule.elect_leader(0, 0),
            AuthorityIndex::new_for_test(0)
        );
        assert_eq!(
            leader_schedule.elect_leader(1, 0),
            AuthorityIndex::new_for_test(1)
        );
        assert_eq!(
            leader_schedule.elect_leader(5, 0),
            AuthorityIndex::new_for_test(1)
        );
        // ensure we elect different leaders for the same round for the multi-leader case
        assert_ne!(
            leader_schedule.elect_leader_stake_based(1, 1),
            leader_schedule.elect_leader_stake_based(1, 2)
        );
    }

    #[test]
    fn test_elect_leader_stake_based() {
        let context = Arc::new(Context::new_for_test(4).0);
        let leader_schedule = LeaderSchedule::new(context, LeaderSwapTable::default());

        assert_eq!(
            leader_schedule.elect_leader_stake_based(0, 0),
            AuthorityIndex::new_for_test(1)
        );
        assert_eq!(
            leader_schedule.elect_leader_stake_based(1, 0),
            AuthorityIndex::new_for_test(1)
        );
        assert_eq!(
            leader_schedule.elect_leader_stake_based(5, 0),
            AuthorityIndex::new_for_test(3)
        );
        // ensure we elect different leaders for the same round for the multi-leader case
        assert_ne!(
            leader_schedule.elect_leader_stake_based(1, 1),
            leader_schedule.elect_leader_stake_based(1, 2)
        );
    }

    #[test]
    fn test_leader_swap_table() {
        telemetry_subscribers::init_for_testing();
        let context = Arc::new(Context::new_for_test(4).0);

        let swap_stake_threshold = 33;
        let reputation_scores = ReputationScores::new(
            CommitRange::new(0..10),
            (0..4).map(|i| i as u64).collect::<Vec<_>>(),
        );
        let leader_swap_table =
            LeaderSwapTable::new(context, reputation_scores, swap_stake_threshold);

        assert_eq!(leader_swap_table.good_nodes.len(), 1);
        assert_eq!(
            leader_swap_table.good_nodes[0].0,
            AuthorityIndex::new_for_test(3)
        );
        assert_eq!(leader_swap_table.bad_nodes.len(), 1);
        assert!(leader_swap_table
            .bad_nodes
            .contains_key(&AuthorityIndex::new_for_test(0)));
    }

    #[test]
    fn test_leader_swap_table_swap() {
        telemetry_subscribers::init_for_testing();
        let context = Arc::new(Context::new_for_test(4).0);

        let swap_stake_threshold = 33;
        let reputation_scores = ReputationScores::new(
            CommitRange::new(0..10),
            (0..4).map(|i| i as u64).collect::<Vec<_>>(),
        );
        let leader_swap_table =
            LeaderSwapTable::new(context.clone(), reputation_scores, swap_stake_threshold);

        // Test swapping a bad leader
        let leader = AuthorityIndex::new_for_test(0);
        let leader_round = 1;
        let leader_offset = 0;
        let swapped_leader = leader_swap_table.swap(leader, leader_round, leader_offset);
        assert_eq!(swapped_leader, Some(AuthorityIndex::new_for_test(3)));

        // Test not swapping a good leader
        let leader = AuthorityIndex::new_for_test(1);
        let leader_round = 1;
        let leader_offset = 0;
        let swapped_leader = leader_swap_table.swap(leader, leader_round, leader_offset);
        assert_eq!(swapped_leader, None);
    }

    #[test]
    fn test_leader_swap_table_retrieve_first_nodes() {
        telemetry_subscribers::init_for_testing();
        let context = Arc::new(Context::new_for_test(4).0);

        let authorities = vec![
            (AuthorityIndex::new_for_test(0), 1),
            (AuthorityIndex::new_for_test(1), 2),
            (AuthorityIndex::new_for_test(2), 3),
            (AuthorityIndex::new_for_test(3), 4),
        ];

        let stake_threshold = 50;
        let filtered_authorities = LeaderSwapTable::retrieve_first_nodes(
            context.clone(),
            authorities.into_iter(),
            stake_threshold,
        );

        // Test setup includes 4 validators with even stake. Therefore with a
        // stake_threshold of 50% we should see 2 validators filtered.
        assert_eq!(filtered_authorities.len(), 2);
        let authority_0_idx = AuthorityIndex::new_for_test(0);
        let authority_0 = context.committee.authority(authority_0_idx);
        assert!(filtered_authorities.contains(&(
            authority_0_idx,
            authority_0.hostname.clone(),
            authority_0.stake
        )));
        let authority_1_idx = AuthorityIndex::new_for_test(1);
        let authority_1 = context.committee.authority(authority_1_idx);
        assert!(filtered_authorities.contains(&(
            authority_1_idx,
            authority_1.hostname.clone(),
            authority_1.stake
        )));
    }

    #[test]
    #[should_panic(
        expected = "The swap_stake_threshold (34) should be in range [0 - 33], out of bounds parameter detected"
    )]
    fn test_leader_swap_table_swap_stake_threshold_out_of_bounds() {
        telemetry_subscribers::init_for_testing();
        let context = Arc::new(Context::new_for_test(4).0);

        let swap_stake_threshold = 34;
        let reputation_scores = ReputationScores::new(
            CommitRange::new(0..10),
            (0..4).map(|i| i as u64).collect::<Vec<_>>(),
        );
        LeaderSwapTable::new(context, reputation_scores, swap_stake_threshold);
    }

    #[test]
    fn test_update_leader_swap_table() {
        telemetry_subscribers::init_for_testing();
        let context = Arc::new(Context::new_for_test(4).0);

        let swap_stake_threshold = 33;
        let reputation_scores = ReputationScores::new(
            CommitRange::new(1..10),
            (0..4).map(|i| i as u64).collect::<Vec<_>>(),
        );
        let leader_swap_table =
            LeaderSwapTable::new(context.clone(), reputation_scores, swap_stake_threshold);

        let leader_schedule = LeaderSchedule::new(context.clone(), LeaderSwapTable::default());

        // Update leader from brand new schedule to first real schedule
        leader_schedule.update_leader_swap_table(leader_swap_table.clone());

        let reputation_scores = ReputationScores::new(
            CommitRange::new(11..20),
            (0..4).map(|i| i as u64).collect::<Vec<_>>(),
        );
        let leader_swap_table =
            LeaderSwapTable::new(context.clone(), reputation_scores, swap_stake_threshold);

        // Update leader from old swap table to new valid swap table
        leader_schedule.update_leader_swap_table(leader_swap_table.clone());
    }

    #[test]
    #[should_panic(
        expected = "The new LeaderSwapTable has an invalid CommitRange. Old LeaderSwapTable CommitRange(11..20) vs new LeaderSwapTable CommitRange(21..25)"
    )]
    fn test_update_bad_leader_swap_table() {
        telemetry_subscribers::init_for_testing();
        let context = Arc::new(Context::new_for_test(4).0);

        let swap_stake_threshold = 33;
        let reputation_scores = ReputationScores::new(
            CommitRange::new(1..10),
            (0..4).map(|i| i as u64).collect::<Vec<_>>(),
        );
        let leader_swap_table =
            LeaderSwapTable::new(context.clone(), reputation_scores, swap_stake_threshold);

        let leader_schedule = LeaderSchedule::new(context.clone(), LeaderSwapTable::default());

        // Update leader from brand new schedule to first real schedule
        leader_schedule.update_leader_swap_table(leader_swap_table.clone());

        let reputation_scores = ReputationScores::new(
            CommitRange::new(11..20),
            (0..4).map(|i| i as u64).collect::<Vec<_>>(),
        );
        let leader_swap_table =
            LeaderSwapTable::new(context.clone(), reputation_scores, swap_stake_threshold);

        // Update leader from old swap table to new valid swap table
        leader_schedule.update_leader_swap_table(leader_swap_table.clone());

        let reputation_scores = ReputationScores::new(
            CommitRange::new(21..25),
            (0..4).map(|i| i as u64).collect::<Vec<_>>(),
        );
        let leader_swap_table =
            LeaderSwapTable::new(context.clone(), reputation_scores, swap_stake_threshold);

        // Update leader from old swap table to new invalid swap table
        leader_schedule.update_leader_swap_table(leader_swap_table.clone());
    }
}
