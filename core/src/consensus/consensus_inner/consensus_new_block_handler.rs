// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use crate::{
    block_data_manager::{BlockDataManager, BlockStatus, LocalBlockInfo},
    consensus::{
        consensus_inner::{
            confirmation_meter::ConfirmationMeter,
            consensus_executor::{ConsensusExecutor, EpochExecutionTask},
            ConsensusGraphInner, NULL,
        },
        debug::ComputeEpochDebugRecord,
        ConsensusConfig,
    },
    parameters::{consensus::*, consensus_internal::*},
    rlp::Encodable,
    statistics::SharedStatistics,
    storage::StateRootWithAuxInfo,
    SharedTransactionPool,
};
use cfx_types::H256;
use hibitset::{BitSet, BitSetLike, DrainableBitSet};
use parity_bytes::ToPretty;
use primitives::{BlockHeader, SignedTransaction};
use std::{
    cmp::{max, min},
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    slice::Iter,
    sync::Arc,
};

pub struct ConsensusNewBlockHandler {
    conf: ConsensusConfig,
    txpool: SharedTransactionPool,
    data_man: Arc<BlockDataManager>,
    executor: Arc<ConsensusExecutor>,
    statistics: SharedStatistics,
}

/// ConsensusNewBlockHandler contains all sub-routines for handling new arriving
/// blocks from network or db. It manipulates and updates ConsensusGraphInner
/// object accordingly.
impl ConsensusNewBlockHandler {
    pub fn new(
        conf: ConsensusConfig, txpool: SharedTransactionPool,
        data_man: Arc<BlockDataManager>, executor: Arc<ConsensusExecutor>,
        statistics: SharedStatistics,
    ) -> Self
    {
        Self {
            conf,
            txpool,
            data_man,
            executor,
            statistics,
        }
    }

    /// Note that there is an important assumption: the timer chain must have no
    /// block in the anticone of new_era_block_arena_index. If this is not
    /// true, it cannot become a checkpoint block
    fn make_checkpoint_at(
        inner: &mut ConsensusGraphInner, new_era_block_arena_index: usize,
        will_execute: bool, executor: &ConsensusExecutor,
    )
    {
        let new_era_height = inner.arena[new_era_block_arena_index].height;
        let new_era_stable_height =
            new_era_height + inner.inner_conf.era_epoch_count;

        let stable_era_genesis =
            inner.get_pivot_block_arena_index(new_era_stable_height);

        // FIXME: I am not sure whether this code still works in the new timer chain checkpoint mechanism
        // (`RecoverBlockFromDb` or `Normal`), ensure all blocks on the
        // pivot chain before stable_era_genesis have state_valid
        // computed
        if will_execute {
            // Make sure state execution is finished before setting lower_bound
            // to the new_checkpoint_era_genesis.
            executor
                .wait_for_result(inner.arena[new_era_block_arena_index].hash);
            inner
                .compute_state_valid(stable_era_genesis)
                .expect("Old cur_era_stable_height has available state_valid");
        }

        // We first compute the set of blocks inside the new era and we
        // recompute the past_weight inside the stable height.
        let mut new_era_block_arena_index_set = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(new_era_block_arena_index);
        new_era_block_arena_index_set.insert(new_era_block_arena_index);
        while let Some(x) = queue.pop_front() {
            for child in &inner.arena[x].children {
                if !new_era_block_arena_index_set.contains(child) {
                    queue.push_back(*child);
                    new_era_block_arena_index_set.insert(*child);
                }
            }
            for referrer in &inner.arena[x].referrers {
                if !new_era_block_arena_index_set.contains(referrer) {
                    queue.push_back(*referrer);
                    new_era_block_arena_index_set.insert(*referrer);
                }
            }
        }
        // This is the arena indices for legacy blocks
        let mut new_era_genesis_subtree = HashSet::new();
        queue.push_back(new_era_block_arena_index);
        while let Some(x) = queue.pop_front() {
            new_era_genesis_subtree.insert(x);
            for child in &inner.arena[x].children {
                queue.push_back(*child);
            }
        }
        let new_era_legacy_block_arena_index_set: HashSet<_> =
            new_era_block_arena_index_set
                .difference(&new_era_genesis_subtree)
                .collect();

        // Now we topologically sort the blocks outside the era
        let mut outside_block_arena_indices = HashSet::new();
        for (index, _) in inner.arena.iter() {
            if !new_era_block_arena_index_set.contains(&index) {
                outside_block_arena_indices.insert(index);
            }
        }
        // Next we are going to recompute all referee and referrer information
        // in arena
        let new_era_pivot_index = inner.height_to_pivot_index(new_era_height);
        for v in new_era_block_arena_index_set.iter() {
            let me = *v;
            inner.arena[me]
                .referees
                .retain(|v| new_era_block_arena_index_set.contains(v));
            inner.arena[me]
                .referrers
                .retain(|v| new_era_block_arena_index_set.contains(v));
            // We no longer need to consider blocks outside our era when
            // computing blockset_in_epoch
            inner.arena[me]
                .data
                .blockset_in_own_view_of_epoch
                .retain(|v| new_era_block_arena_index_set.contains(v));
            if !new_era_block_arena_index_set
                .contains(&inner.arena[me].last_timer_block_arena_index)
            {
                inner.arena[me].last_timer_block_arena_index = NULL;
            }
            if !new_era_block_arena_index_set
                .contains(&inner.arena[me].data.force_confirm)
            {
                inner.arena[me].data.force_confirm = new_era_block_arena_index;
            }
        }
        // reassign the parent for outside era blocks
        for v in new_era_legacy_block_arena_index_set {
            let me = *v;
            let mut parent = inner.arena[me].parent;
            if inner.arena[me].era_block != NULL {
                inner.split_root(me);
            }
            if !new_era_block_arena_index_set.contains(&parent) {
                parent = new_era_block_arena_index;
            }
            inner.arena[me].parent = parent;
            inner.arena[me].era_block = NULL;
        }
        // Now we are ready to cleanup outside blocks in inner data structures
        {
            let mut old_era_block_set = inner.old_era_block_set.lock();
            inner
                .pastset_cache
                .intersect_update(&outside_block_arena_indices);
            for index in outside_block_arena_indices {
                let hash = inner.arena[index].hash;
                old_era_block_set.push_back(hash);
                inner.hash_to_arena_indices.remove(&hash);
                inner.terminal_hashes.remove(&hash);
                inner.arena.remove(index);
                // remove useless data in BlockDataManager
                inner.data_man.remove_epoch_execution_commitment(&hash);
                inner.data_man.remove_epoch_execution_context(&hash);
                inner.transaction_caches.remove(&index);
            }
        }

        let mut timer_chain_truncate = 0;
        while timer_chain_truncate < inner.timer_chain.len()
            && !new_era_block_arena_index_set
                .contains(&inner.timer_chain[timer_chain_truncate])
        {
            timer_chain_truncate += 1;
        }
        inner.cur_era_genesis_timer_chain_height += timer_chain_truncate as u64;
        assert_eq!(
            inner.cur_era_genesis_timer_chain_height,
            inner.arena[new_era_block_arena_index].timer_chain_height
        );
        for i in 0..(inner.timer_chain.len() - timer_chain_truncate) {
            inner.timer_chain[i] = inner.timer_chain[i + timer_chain_truncate];
            if i + timer_chain_truncate
                < inner.timer_chain_accumulative_lca.len()
            {
                inner.timer_chain_accumulative_lca[i] = inner
                    .timer_chain_accumulative_lca[i + timer_chain_truncate];
            }
        }
        inner
            .timer_chain
            .resize(inner.timer_chain.len() - timer_chain_truncate, 0);
        if inner.timer_chain_accumulative_lca.len() > timer_chain_truncate {
            inner.timer_chain_accumulative_lca.resize(
                inner.timer_chain_accumulative_lca.len() - timer_chain_truncate,
                0,
            );
        } else {
            inner.timer_chain_accumulative_lca.clear();
        }
        for i in 0..(inner.inner_conf.timer_chain_beta as usize - 1) {
            if i < inner.timer_chain_accumulative_lca.len() {
                inner.timer_chain_accumulative_lca[i] =
                    new_era_block_arena_index;
            }
        }

        assert!(new_era_pivot_index < inner.pivot_chain.len());
        inner.pivot_chain = inner.pivot_chain.split_off(new_era_pivot_index);
        inner.pivot_chain_metadata =
            inner.pivot_chain_metadata.split_off(new_era_pivot_index);
        for d in inner.pivot_chain_metadata.iter_mut() {
            d.last_pivot_in_past_blocks
                .retain(|v| new_era_block_arena_index_set.contains(v));
        }
        inner
            .anticone_cache
            .intersect_update(&new_era_genesis_subtree);

        // Chop off all link-cut-trees in the inner data structure
        inner.split_root(new_era_block_arena_index);

        inner.cur_era_genesis_block_arena_index = new_era_block_arena_index;
        inner.cur_era_genesis_height = new_era_height;

        // TODO: maybe archive node has other logic.
        {
            let state_availability_boundary =
                &mut *inner.data_man.state_availability_boundary.write();
            if new_era_height > state_availability_boundary.lower_bound {
                state_availability_boundary.adjust_lower_bound(new_era_height);
            }
        }

        let cur_era_hash = inner.arena[new_era_block_arena_index].hash.clone();
        let next_era_arena_index =
            inner.pivot_chain[inner.inner_conf.era_epoch_count as usize];
        let next_era_hash = inner.arena[next_era_arena_index].hash.clone();

        // This must be true given our checkpoint rule!
        for (_, x) in &inner.invalid_block_queue {
            assert!(new_era_block_arena_index_set.contains(x))
        }

        inner
            .data_man
            .set_cur_consensus_era_genesis_hash(&cur_era_hash, &next_era_hash);
    }

    pub fn compute_anticone_bruteforce(
        inner: &ConsensusGraphInner, me: usize,
    ) -> BitSet {
        let parent = inner.arena[me].parent;
        if parent == NULL {
            // This is genesis, so the anticone should be empty
            return BitSet::new();
        }
        let mut last_in_pivot = inner.arena[parent].last_pivot_in_past;
        for referee in &inner.arena[me].referees {
            last_in_pivot =
                max(last_in_pivot, inner.arena[*referee].last_pivot_in_past);
        }
        let mut visited = BitSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(me);
        visited.add(me as u32);
        while let Some(index) = queue.pop_front() {
            let parent = inner.arena[index].parent;
            if parent != NULL
                && inner.arena[parent].data.epoch_number > last_in_pivot
                && !visited.contains(parent as u32)
            {
                visited.add(parent as u32);
                queue.push_back(parent);
            }
            for referee in &inner.arena[index].referees {
                if inner.arena[*referee].data.epoch_number > last_in_pivot
                    && !visited.contains(*referee as u32)
                {
                    visited.add(*referee as u32);
                    queue.push_back(*referee);
                }
            }
        }
        // Now we traverse all future of me, when adding new block, this is
        // empty
        queue.clear();
        queue.push_back(me);
        while let Some(index) = queue.pop_front() {
            for child in &inner.arena[index].children {
                if !visited.contains(*child as u32) {
                    visited.add(*child as u32);
                    queue.push_back(*child);
                }
            }
            for referrer in &inner.arena[index].referrers {
                if !visited.contains(*referrer as u32) {
                    visited.add(*referrer as u32);
                    queue.push_back(*referrer);
                }
            }
        }

        let mut anticone = BitSet::with_capacity(inner.arena.capacity() as u32);
        for (i, node) in inner.arena.iter() {
            if node.data.epoch_number > last_in_pivot
                && !visited.contains(i as u32)
                && node.data.active_cnt == 0
            {
                anticone.add(i as u32);
            }
        }
        anticone
    }

    fn compute_anticone(
        inner: &mut ConsensusGraphInner, me: usize,
    ) -> (BitSet, BitSet) {
        let parent = inner.arena[me].parent;
        debug_assert!(parent != NULL);
        debug_assert!(inner.arena[me].children.is_empty());
        debug_assert!(inner.arena[me].referrers.is_empty());

        // If we do not have the anticone of its parent, we compute it with
        // brute force!
        let parent_anticone_opt = inner.anticone_cache.get(parent);
        let mut anticone;
        if parent_anticone_opt.is_none() {
            anticone = ConsensusNewBlockHandler::compute_anticone_bruteforce(
                inner, me,
            );
        } else {
            // anticone = parent_anticone + parent_future - my_past
            // Compute future set of parent
            anticone = inner.compute_future_bitset(parent);
            anticone.remove(me as u32);

            for index in parent_anticone_opt.unwrap() {
                anticone.add(*index as u32);
            }
            let mut my_past = BitSet::new();
            let mut queue: VecDeque<usize> = VecDeque::new();
            queue.push_back(me);
            while let Some(index) = queue.pop_front() {
                if my_past.contains(index as u32) {
                    continue;
                }

                debug_assert!(index != parent);
                if index != me {
                    my_past.add(index as u32);
                }

                let idx_parent = inner.arena[index].parent;
                debug_assert!(idx_parent != NULL);
                if anticone.contains(idx_parent as u32) {
                    queue.push_back(idx_parent);
                }

                for referee in &inner.arena[index].referees {
                    if anticone.contains(*referee as u32) {
                        queue.push_back(*referee);
                    }
                }
            }
            for index in my_past.drain() {
                anticone.remove(index);
            }
        }

        // We only consider non-lagacy blocks when computing anticone.
        for index in anticone.clone().iter() {
            if inner.arena[index as usize].era_block == NULL {
                anticone.remove(index);
            }
        }

        inner.anticone_cache.update(me, &anticone);

        let mut anticone_barrier = BitSet::new();
        for index in anticone.clone().iter() {
            let parent = inner.arena[index as usize].parent as u32;
            if !anticone.contains(parent) {
                anticone_barrier.add(index);
            }
        }

        debug!(
            "Block {} anticone size {}",
            inner.arena[me].hash,
            anticone.len()
        );

        (anticone, anticone_barrier)
    }

    fn check_correct_parent_brutal(
        inner: &ConsensusGraphInner, me: usize, subtree_weight: &Vec<i128>,
        checking_candidate: Iter<usize>,
    ) -> bool
    {
        let mut valid = true;
        let parent = inner.arena[me].parent;
        let force_confirm = inner.arena[me].data.force_confirm;
        let force_confirm_height = inner.arena[force_confirm].height;

        // Check the pivot selection decision.
        for consensus_arena_index_in_epoch in checking_candidate {
            let lca = inner.lca(*consensus_arena_index_in_epoch, parent);
            assert!(lca != *consensus_arena_index_in_epoch);
            // If it is outside current era, we will skip!
            if lca == NULL || inner.arena[lca].height < force_confirm_height {
                continue;
            }
            if lca == parent {
                valid = false;
                break;
            }

            let fork = inner.ancestor_at(
                *consensus_arena_index_in_epoch,
                inner.arena[lca].height + 1,
            );
            let pivot = inner.ancestor_at(parent, inner.arena[lca].height + 1);

            let fork_subtree_weight = subtree_weight[fork];
            let pivot_subtree_weight = subtree_weight[pivot];

            if ConsensusGraphInner::is_heavier(
                (fork_subtree_weight, &inner.arena[fork].hash),
                (pivot_subtree_weight, &inner.arena[pivot].hash),
            ) {
                valid = false;
                break;
            }
        }

        valid
    }

    fn check_correct_parent(
        inner: &mut ConsensusGraphInner, me: usize, anticone_barrier: &BitSet,
        weight_tuple: Option<&Vec<i128>>,
    ) -> bool
    {
        let parent = inner.arena[me].parent;
        // FIXME: Because now we allow partial invalid blocks as parent, we need
        // to consider more for block candidates. This may cause a
        // performance issue and we should consider another optimized strategy.
        let mut candidate;
        let candidate_iter = if inner.arena[parent].data.partial_invalid {
            candidate =
                inner.arena[me].data.blockset_in_own_view_of_epoch.clone();
            let mut p = parent;
            while p != NULL && inner.arena[p].data.partial_invalid {
                candidate.extend(
                    inner.arena[p].data.blockset_in_own_view_of_epoch.iter(),
                );
                p = inner.arena[p].parent;
            }
            candidate.iter()
        } else {
            inner.arena[me].data.blockset_in_own_view_of_epoch.iter()
        };

        if let Some(subtree_weight) = weight_tuple {
            return ConsensusNewBlockHandler::check_correct_parent_brutal(
                inner,
                me,
                subtree_weight,
                candidate_iter,
            );
        }
        let mut valid = true;
        let force_confirm = inner.arena[me].data.force_confirm;
        let force_confirm_height = inner.arena[force_confirm].height;
        //        debug!("force confirm {} height {}", force_confirm,
        // force_confirm_height);

        let mut weight_delta = HashMap::new();

        for index in anticone_barrier {
            weight_delta
                .insert(index as usize, inner.weight_tree.get(index as usize));
        }

        // Remove weight contribution of anticone
        for (index, delta) in &weight_delta {
            inner.weight_tree.path_apply(*index, -delta);
        }

        //        debug!("BLOCKSET {:?} len {}",
        // inner.arena[me].data.blockset_in_own_view_of_epoch,
        // inner.arena[me].data.blockset_in_own_view_of_epoch.len());
        // Check the pivot selection decision.
        for consensus_arena_index_in_epoch in candidate_iter {
            let lca = inner.lca(*consensus_arena_index_in_epoch, parent);
            assert!(lca != *consensus_arena_index_in_epoch);
            // debug!("checking lca {}", lca);
            // If it is outside the era, we will skip!
            if lca == NULL || inner.arena[lca].height < force_confirm_height {
                continue;
            }
            if lca == parent {
                valid = false;
                break;
            }

            let fork = inner.ancestor_at(
                *consensus_arena_index_in_epoch,
                inner.arena[lca].height + 1,
            );
            let pivot = inner.ancestor_at(parent, inner.arena[lca].height + 1);

            let fork_subtree_weight = inner.weight_tree.get(fork);
            let pivot_subtree_weight = inner.weight_tree.get(pivot);

            // debug!("checking lca {} fork {} fork_weight {} pivot_weight {}",
            // lca, fork, fork_subtree_weight, pivot_subtree_weight);
            if ConsensusGraphInner::is_heavier(
                (fork_subtree_weight, &inner.arena[fork].hash),
                (pivot_subtree_weight, &inner.arena[pivot].hash),
            ) {
                valid = false;
                break;
            }
        }

        for (index, delta) in &weight_delta {
            inner.weight_tree.path_apply(*index, *delta);
        }

        valid
    }

    #[allow(dead_code)]
    fn log_debug_epoch_computation(
        &self, epoch_arena_index: usize, inner: &mut ConsensusGraphInner,
    ) -> ComputeEpochDebugRecord {
        let epoch_block_hash = inner.arena[epoch_arena_index].hash;

        let epoch_block_hashes =
            inner.get_epoch_block_hashes(epoch_arena_index);

        // Parent state root.
        let parent_arena_index = inner.arena[epoch_arena_index].parent;
        let parent_block_hash = inner.arena[parent_arena_index].hash;
        let parent_state_root = inner
            .data_man
            .get_epoch_execution_commitment(&parent_block_hash)
            .unwrap()
            .state_root_with_aux_info
            .clone();

        let reward_index = inner.get_pivot_reward_index(epoch_arena_index);

        let reward_execution_info = self
            .executor
            .get_reward_execution_info_from_index(inner, reward_index);
        let task = EpochExecutionTask::new(
            epoch_block_hash,
            epoch_block_hashes.clone(),
            inner.get_epoch_start_block_number(epoch_arena_index),
            reward_execution_info,
            false,
            true,
        );
        let debug_record_data = task.debug_record.clone();
        {
            let mut debug_record_data_locked = debug_record_data.lock();
            let debug_record = debug_record_data_locked.as_mut().unwrap();

            debug_record.parent_block_hash = parent_block_hash;
            debug_record.parent_state_root = parent_state_root;
            debug_record.reward_epoch_hash =
                if let Some((reward_epoch_block, _)) = reward_index.clone() {
                    Some(inner.arena[reward_epoch_block].hash)
                } else {
                    None
                };
            debug_record.anticone_penalty_cutoff_epoch_hash =
                if let Some((_, anticone_penalty_cutoff_epoch_block)) =
                    reward_index.clone()
                {
                    Some(inner.arena[anticone_penalty_cutoff_epoch_block].hash)
                } else {
                    None
                };

            let blocks = epoch_block_hashes
                .iter()
                .map(|hash| {
                    self.data_man
                        .block_by_hash(hash, false /* update_cache */)
                        .unwrap()
                })
                .collect::<Vec<_>>();

            debug_record.block_hashes = epoch_block_hashes;
            debug_record.block_txs = blocks
                .iter()
                .map(|block| block.transactions.len())
                .collect::<Vec<_>>();
            debug_record.transactions = blocks
                .iter()
                .flat_map(|block| block.transactions.clone())
                .collect::<Vec<_>>();

            debug_record.block_authors = blocks
                .iter()
                .map(|block| *block.block_header.author())
                .collect::<Vec<_>>();
        }
        self.executor.enqueue_epoch(task);
        self.executor.wait_for_result(epoch_block_hash);

        Arc::try_unwrap(debug_record_data)
            .unwrap()
            .into_inner()
            .unwrap()
    }

    #[allow(dead_code)]
    fn log_invalid_state_root(
        &self, expected_state_root: &StateRootWithAuxInfo,
        got_state_root: &StateRootWithAuxInfo, deferred: usize,
        inner: &mut ConsensusGraphInner,
    ) -> std::io::Result<()>
    {
        let debug_record = self.log_debug_epoch_computation(deferred, inner);
        let debug_record_rlp = debug_record.rlp_bytes();

        let deferred_block_hash = inner.arena[deferred].hash;

        warn!(
            "Invalid state root: should be {:?}, got {:?}, deferred block: {:?}, \
            reward epoch bock: {:?}, anticone cutoff block: {:?}, \
            number of blocks in epoch: {:?}, number of transactions in epoch: {:?}, rewards: {:?}",
            expected_state_root,
            got_state_root,
            deferred_block_hash,
            debug_record.reward_epoch_hash,
            debug_record.anticone_penalty_cutoff_epoch_hash,
            debug_record.block_hashes.len(),
            debug_record.transactions.len(),
            debug_record.merged_rewards_by_author,
        );

        let dump_dir = &self.conf.debug_dump_dir_invalid_state_root;
        let invalid_state_root_path =
            dump_dir.clone() + &deferred_block_hash.to_hex();
        std::fs::create_dir_all(dump_dir)?;

        if std::path::Path::new(&invalid_state_root_path).exists() {
            return Ok(());
        }
        let mut file = std::fs::File::create(&invalid_state_root_path)?;
        file.write_all(&debug_record_rlp)?;

        Ok(())
    }

    fn check_block_full_validity(
        &self, new: usize, inner: &mut ConsensusGraphInner, adaptive: bool,
        anticone_barrier: &BitSet, weight_tuple: Option<&Vec<i128>>,
    ) -> bool
    {
        let parent = inner.arena[new].parent;
        let force_confirm = inner.arena[new].data.force_confirm;

        if inner.lca(parent, force_confirm) != force_confirm {
            warn!("Partially invalid due to picking incorrect parent (force confirmation {:?} violation). {:?}", force_confirm, inner.arena[new].hash);
            return false;
        }

        // Check whether the new block select the correct parent block
        if !ConsensusNewBlockHandler::check_correct_parent(
            inner,
            new,
            anticone_barrier,
            weight_tuple,
        ) {
            warn!(
                "Partially invalid due to picking incorrect parent. {:?}",
                inner.arena[new].hash
            );
            return false;
        }

        // Check whether difficulty is set correctly
        if inner.arena[new].difficulty
            != inner.expected_difficulty(&inner.arena[parent].hash)
        {
            warn!(
                "Partially invalid due to wrong difficulty. {:?}",
                inner.arena[new].hash
            );
            return false;
        }

        // Check adaptivity match. Note that in bench mode we do not check
        // the adaptive field correctness. We simply override its value
        // with the right one.
        if !self.conf.bench_mode {
            if inner.arena[new].adaptive != adaptive {
                warn!(
                    "Partially invalid due to invalid adaptive field. {:?}",
                    inner.arena[new].hash
                );
                return false;
            }
        }

        return true;
    }

    /// Subroutine called by on_new_block()
    fn insert_block_initial(
        &self, inner: &mut ConsensusGraphInner, block_header: &BlockHeader,
    ) -> usize {
        let (me, indices_len) = inner.insert(&block_header);
        self.statistics
            .set_consensus_graph_inserted_block_count(indices_len);
        me
    }

    /// Subroutine called by on_new_block()
    fn update_lcts_initial(&self, inner: &mut ConsensusGraphInner, me: usize) {
        let parent = inner.arena[me].parent;

        inner.weight_tree.make_tree(me);
        inner.weight_tree.link(parent, me);

        inner.adaptive_tree.make_tree(me);
        inner.adaptive_tree.link(parent, me);
        let parent_tw = inner.weight_tree.get(parent);
        let parent_w = inner.block_weight(parent);
        inner.adaptive_tree.set(me, -parent_tw + parent_w);
    }

    /// Subroutine called by on_new_block()
    fn update_lcts_finalize(
        &self, inner: &mut ConsensusGraphInner, me: usize,
    ) -> i128 {
        let parent = inner.arena[me].parent;
        let weight = inner.block_weight(me);

        inner.weight_tree.path_apply(me, weight);

        inner.adaptive_tree.path_apply(me, 2 * weight);
        inner.adaptive_tree.caterpillar_apply(parent, -weight);

        weight
    }

    fn process_outside_block(
        &self, inner: &mut ConsensusGraphInner, block_header: &BlockHeader,
    ) -> u64 {
        inner.insert_out_era_block(block_header)
    }

    fn recycle_tx_in_block(
        &self, inner: &ConsensusGraphInner, arena_index: usize,
    ) {
        let block = inner
            .data_man
            .block_by_hash(
                &inner.arena[arena_index].hash,
                true, /* update_cache */
            )
            .expect("Block should always found in the data manager!");
        self.txpool.recycle_transactions(block.transactions.clone());
    }

    /// This recycles txs in all blocks outside the era represented by the era
    /// block.
    fn recycle_tx_outside_era(
        &self, inner: &mut ConsensusGraphInner, era_block: usize,
    ) {
        let mut anticone_tmp = HashSet::new();
        let anticone = if let Some(x) = inner.anticone_cache.get(era_block) {
            x
        } else {
            let anticone_bitset =
                ConsensusNewBlockHandler::compute_anticone_bruteforce(
                    inner, era_block,
                );
            for idx in anticone_bitset.iter() {
                anticone_tmp.insert(idx as usize);
            }
            &anticone_tmp
        };

        for idx in anticone.iter() {
            self.recycle_tx_in_block(inner, *idx);
        }

        let future = inner.compute_future_bitset(era_block);
        for idx in future.iter() {
            let index = idx as usize;
            let lca = inner.lca(index, era_block);
            if lca != era_block {
                self.recycle_tx_in_block(inner, index);
            }
        }
    }

    fn should_move_stable_height(
        &self, inner: &mut ConsensusGraphInner,
    ) -> u64 {
        let new_stable_height =
            inner.cur_era_stable_height + inner.inner_conf.era_epoch_count;
        // We make sure there is an additional era before the best for moving it
        if new_stable_height + inner.inner_conf.era_epoch_count
            >= inner.best_epoch_number()
        {
            return inner.cur_era_stable_height;
        }
        let new_stable_pivot_arena_index =
            inner.get_pivot_block_arena_index(new_stable_height);
        // Now we need to make sure that this new stable block is
        // force_confirmed in our current graph
        if inner.timer_chain_accumulative_lca.len() == 0 {
            return inner.cur_era_stable_height;
        }
        if let Some(last) = inner.timer_chain_accumulative_lca.last() {
            let lca = inner.lca(*last, new_stable_pivot_arena_index);
            if lca == new_stable_pivot_arena_index {
                return new_stable_height;
            }
        }
        return inner.cur_era_stable_height;
    }

    fn should_form_checkpoint_at(
        &self, inner: &mut ConsensusGraphInner,
    ) -> usize {
        let new_genesis_height =
            inner.cur_era_genesis_height + inner.inner_conf.era_epoch_count;
        // We cannot move beyond the stable block/height
        if new_genesis_height + inner.inner_conf.era_epoch_count
            >= inner.cur_era_stable_height
        {
            return inner.cur_era_genesis_block_arena_index;
        }

        let new_genesis_block_arena_index =
            inner.get_pivot_block_arena_index(new_genesis_height);
        let stable_pivot_block =
            inner.get_pivot_block_arena_index(inner.cur_era_stable_height);
        assert!(inner.arena[stable_pivot_block].data.force_confirm != NULL);
        if inner.lca(
            new_genesis_block_arena_index,
            inner.arena[stable_pivot_block].data.force_confirm,
        ) != new_genesis_block_arena_index
        {
            return inner.cur_era_genesis_block_arena_index;
        }

        // Now we need to make sure that no timer chain block is in the anticone
        // of the new genesis. This is required for our checkpoint
        // algorithm.
        let mut visited = BitSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(new_genesis_block_arena_index);
        while let Some(x) = queue.pop_front() {
            for child in &inner.arena[x].children {
                if !visited.contains(*child as u32) {
                    visited.add(*child as u32);
                    queue.push_back(*child);
                }
            }
            for referrer in &inner.arena[x].referrers {
                if !visited.contains(*referrer as u32) {
                    visited.add(*referrer as u32);
                    queue.push_back(*referrer);
                }
            }
        }
        let start_timer_chain_height =
            inner.arena[new_genesis_block_arena_index].timer_chain_height;
        let start_timer_chain_index = (start_timer_chain_height
            - inner.cur_era_genesis_timer_chain_height)
            as usize
            + 1;
        for i in start_timer_chain_index..inner.timer_chain.len() {
            if !visited.contains(inner.timer_chain[i] as u32) {
                return inner.cur_era_genesis_block_arena_index;
            }
        }
        return new_genesis_block_arena_index;
    }

    fn persist_terminals(&self, inner: &ConsensusGraphInner) {
        let mut terminals = Vec::with_capacity(inner.terminal_hashes.len());
        for h in &inner.terminal_hashes {
            terminals.push(h.clone());
        }
        self.data_man.insert_terminals_to_db(terminals);
    }

    fn try_clear_blockset_in_own_view_of_epoch(
        inner: &mut ConsensusGraphInner, me: usize,
    ) {
        if inner.arena[me].data.blockset_in_own_view_of_epoch.len() as u64
            > BLOCKSET_IN_OWN_VIEW_OF_EPOCH_CAP
        {
            inner.arena[me].data.blockset_in_own_view_of_epoch =
                Default::default();
            inner.arena[me].data.ordered_executable_epoch_blocks =
                Default::default();
            inner.arena[me].data.blockset_cleared = true;
        }
    }

    // This function computes the timer chain in the view of the new block.
    // The first returned value is the fork height of the timer chain.
    // The second is a map that overwrites timer_chain_height values after the
    // fork height.
    fn compute_timer_chain_tuple(
        inner: &ConsensusGraphInner, me: usize, anticone: &BitSet,
    ) -> (u64, HashMap<usize, u64>, Vec<usize>, Vec<usize>) {
        inner.compute_timer_chain_tuple(me, Some(anticone))
    }

    fn preactivate_block(
        &self, inner: &mut ConsensusGraphInner, me: usize,
    ) -> BlockStatus {
        let parent = inner.arena[me].parent;
        let outside_stable_tree = {
            // It's pending if it has a different stable block or is before our
            // stable block or we are still recovering
            let me_stable_arena_index =
                inner.ancestor_at(parent, inner.cur_era_stable_height);
            (inner.pivot_chain.len() as u64 - 1) + inner.cur_era_genesis_height
                < inner.cur_era_stable_height
                || me_stable_arena_index
                    != inner.get_pivot_block_arena_index(
                        inner.cur_era_stable_height,
                    )
        };
        let stable_genesis_in_past = {
            let mut last_pivot_in_past = if parent != NULL {
                inner.arena[parent].height
            } else {
                inner.cur_era_genesis_height
            };
            for referee in &inner.arena[me].referees {
                last_pivot_in_past = max(
                    last_pivot_in_past,
                    inner.arena[*referee].last_pivot_in_past,
                );
            }
            last_pivot_in_past >= inner.cur_era_stable_height
        };

        let pending = outside_stable_tree && !stable_genesis_in_past;

        // Because the following computation relies on all previous blocks being
        // active, We have to delay it till now
        let mut timer_longest_difficulty = 0;
        let mut longest_referee = parent;
        if parent != NULL {
            timer_longest_difficulty = inner.arena[parent]
                .timer_longest_difficulty
                + inner.get_timer_difficulty(parent);
        }
        for referee in &inner.arena[me].referees {
            let timer_difficulty = inner.arena[*referee]
                .timer_longest_difficulty
                + inner.get_timer_difficulty(*referee);
            if longest_referee == NULL
                || ConsensusGraphInner::is_heavier(
                    (timer_difficulty, &inner.arena[*referee].hash),
                    (
                        timer_longest_difficulty,
                        &inner.arena[longest_referee].hash,
                    ),
                )
            {
                timer_longest_difficulty = timer_difficulty;
                longest_referee = *referee;
            }
        }
        let last_timer_block_arena_index = if longest_referee == NULL
            || inner.arena[longest_referee].is_timer
                && !inner.arena[longest_referee].data.partial_invalid
        {
            longest_referee
        } else {
            inner.arena[longest_referee].last_timer_block_arena_index
        };
        debug!("For Block {} Last timer arena index {} longest referee from {} diff {}", me, last_timer_block_arena_index, longest_referee, me);
        inner.arena[me].timer_longest_difficulty = timer_longest_difficulty;
        inner.arena[me].last_timer_block_arena_index =
            last_timer_block_arena_index;

        let (anticone, anticone_barrier) =
            ConsensusNewBlockHandler::compute_anticone(inner, me);
        let timer_chain_tuple =
            ConsensusNewBlockHandler::compute_timer_chain_tuple(
                inner, me, &anticone,
            );
        inner.arena[me].data.force_confirm =
            inner.compute_force_confirm(Some(&timer_chain_tuple));

        let weight_tuple = if anticone_barrier.len() >= ANTICONE_BARRIER_CAP {
            Some(inner.compute_subtree_weights(me, &anticone_barrier))
        } else {
            None
        };

        self.update_lcts_initial(inner, me);

        let mut fully_valid = true;
        if !pending {
            let adaptive = inner.adaptive_weight(
                me,
                &anticone_barrier,
                weight_tuple.as_ref(),
                &timer_chain_tuple,
            );

            debug!(
                "force confirm point is {} in the past view of {}",
                inner.arena[me].data.force_confirm, me
            );
            fully_valid = self.check_block_full_validity(
                me,
                inner,
                adaptive,
                &anticone_barrier,
                weight_tuple.as_ref(),
            );

            if self.conf.bench_mode && fully_valid {
                inner.arena[me].adaptive = adaptive;
            }
        }

        let block_status = if pending {
            BlockStatus::Pending
        } else if fully_valid {
            BlockStatus::Valid
        } else {
            BlockStatus::PartialInvalid
        };
        block_status
    }

    fn activate_block(
        &self, inner: &mut ConsensusGraphInner, me: usize,
        meter: &ConfirmationMeter,
        transactions: Option<Vec<Arc<SignedTransaction>>>,
        queue: &mut VecDeque<usize>,
    )
    {
        debug!(
            "Start activating block in ConsensusGraph: index = {:?} hash={:?}",
            me, inner.arena[me].hash
        );
        let parent = inner.arena[me].parent;
        let has_transactions = transactions.is_some();
        // Update terminal hashes for mining
        if parent != NULL {
            inner.terminal_hashes.remove(&inner.arena[parent].hash);
        }
        inner.terminal_hashes.insert(inner.arena[me].hash.clone());
        for referee in &inner.arena[me].referees {
            inner.terminal_hashes.remove(&inner.arena[*referee].hash);
        }

        let my_weight = self.update_lcts_finalize(inner, me);
        let mut extend_pivot = false;
        let mut pivot_changed = false;
        let mut fork_at;
        let old_pivot_chain_len = inner.pivot_chain.len();

        // Now we are going to maintain the timer chain.
        let diff = inner.arena[me].timer_longest_difficulty
            + inner.get_timer_difficulty(me);
        if inner.arena[me].is_timer
            && !inner.arena[me].data.partial_invalid
            && ConsensusGraphInner::is_heavier(
                (diff, &inner.arena[me].hash),
                (
                    inner.best_timer_chain_difficulty,
                    &inner.best_timer_chain_hash,
                ),
            )
        {
            inner.best_timer_chain_difficulty = diff;
            inner.best_timer_chain_hash = inner.arena[me].hash.clone();
            inner.update_timer_chain(me);
        } else {
            let mut timer_chain_height = inner.arena[parent].timer_chain_height;
            if inner.get_timer_chain_index(parent) != NULL {
                timer_chain_height += 1;
            }
            for referee in &inner.arena[me].referees {
                let timer_bit = if inner.get_timer_chain_index(*referee) != NULL
                {
                    1
                } else {
                    0
                };
                if inner.arena[*referee].timer_chain_height + timer_bit
                    > timer_chain_height
                {
                    timer_chain_height =
                        inner.arena[*referee].timer_chain_height + timer_bit;
                }
            }
            inner.arena[me].timer_chain_height = timer_chain_height;
        }

        meter.aggregate_total_weight_in_past(my_weight);
        let force_confirm = inner.compute_force_confirm(None);
        let force_height = inner.arena[force_confirm].height;
        let last = inner.pivot_chain.last().cloned().unwrap();
        let force_lca = inner.lca(force_confirm, last);

        if force_lca == force_confirm && inner.arena[me].parent == last {
            inner.pivot_chain.push(me);
            inner.set_epoch_number_in_epoch(
                me,
                inner.pivot_index_to_height(inner.pivot_chain.len()) - 1,
            );
            inner.pivot_chain_metadata.push(Default::default());
            extend_pivot = true;
            pivot_changed = true;
            fork_at = inner.pivot_index_to_height(old_pivot_chain_len)
        } else {
            let lca = inner.lca(last, me);
            let new;
            if force_confirm != force_lca {
                fork_at = inner.arena[force_lca].height + 1;
                new = inner.ancestor_at(force_confirm, fork_at);
                pivot_changed = true;
            } else {
                fork_at = inner.arena[lca].height + 1;
                let prev = inner.get_pivot_block_arena_index(fork_at);
                let prev_weight = inner.weight_tree.get(prev);
                new = inner.ancestor_at(me, fork_at);
                let new_weight = inner.weight_tree.get(new);

                if ConsensusGraphInner::is_heavier(
                    (new_weight, &inner.arena[new].hash),
                    (prev_weight, &inner.arena[prev].hash),
                ) {
                    pivot_changed = true;
                } else {
                    // The previous subtree is still heavier, nothing is
                    // updated
                    debug!("Old pivot chain is heavier, pivot chain unchanged");
                    fork_at = inner.pivot_index_to_height(old_pivot_chain_len);
                }
            }
            if pivot_changed {
                // The new subtree is heavier, update pivot chain
                let fork_pivot_index = inner.height_to_pivot_index(fork_at);
                assert!(fork_pivot_index < inner.pivot_chain.len());
                for discarded_idx in
                    inner.pivot_chain.split_off(fork_pivot_index)
                {
                    // Reset the epoch_number of the discarded fork
                    inner.reset_epoch_number_in_epoch(discarded_idx);
                    ConsensusNewBlockHandler::try_clear_blockset_in_own_view_of_epoch(inner, discarded_idx);
                }
                let mut u = new;
                loop {
                    if inner.arena[u].data.blockset_cleared {
                        inner.collect_blockset_in_own_view_of_epoch(u);
                    }
                    inner.pivot_chain.push(u);
                    inner.set_epoch_number_in_epoch(
                        u,
                        inner.pivot_index_to_height(inner.pivot_chain.len())
                            - 1,
                    );
                    if inner.arena[u].height >= force_height {
                        let mut heaviest = NULL;
                        let mut heaviest_weight = 0;
                        for index in &inner.arena[u].children {
                            if inner.arena[*index].data.active_cnt != 0 {
                                continue;
                            }
                            let weight = inner.weight_tree.get(*index);
                            if heaviest == NULL
                                || ConsensusGraphInner::is_heavier(
                                    (weight, &inner.arena[*index].hash),
                                    (
                                        heaviest_weight,
                                        &inner.arena[heaviest].hash,
                                    ),
                                )
                            {
                                heaviest = *index;
                                heaviest_weight = weight;
                            }
                        }
                        if heaviest == NULL {
                            break;
                        }
                        u = heaviest;
                    } else {
                        u = inner.ancestor_at(
                            force_confirm,
                            inner.arena[u].height + 1,
                        );
                    }
                }
            }
        };
        debug!(
            "Forked at height {}, fork parent block {} index {}",
            fork_at,
            &inner.arena[inner.get_pivot_block_arena_index(fork_at - 1)].hash,
            parent,
        );

        // Now compute last_pivot_in_block and update pivot_metadata.
        // Note that we need to do this for partially invalid blocks to
        // propagate information!
        if !extend_pivot {
            let update_at = fork_at - 1;
            let mut last_pivot_to_update = HashSet::new();
            last_pivot_to_update.insert(me);
            if pivot_changed {
                let update_pivot_index = inner.height_to_pivot_index(update_at);
                for pivot_index in update_pivot_index..old_pivot_chain_len {
                    for x in &inner.pivot_chain_metadata[pivot_index]
                        .last_pivot_in_past_blocks
                    {
                        last_pivot_to_update.insert(*x);
                    }
                }
                inner.recompute_metadata(fork_at, last_pivot_to_update);
            } else {
                // pivot chain not extend and not change
                ConsensusNewBlockHandler::try_clear_blockset_in_own_view_of_epoch(inner, me);
                inner.recompute_metadata(
                    inner.get_pivot_height(),
                    last_pivot_to_update,
                );
            }
        } else {
            let height = inner.arena[me].height;
            inner.arena[me].last_pivot_in_past = height;
            let pivot_index = inner.height_to_pivot_index(height);
            inner.pivot_chain_metadata[pivot_index]
                .last_pivot_in_past_blocks
                .insert(me);
        }

        let mut concat_list = inner.arena[me].children.clone();
        concat_list.extend(inner.arena[me].referrers.iter());
        for succ in &concat_list {
            assert!(inner.arena[*succ].data.active_cnt > 0);
            inner.arena[*succ].data.active_cnt -= 1;
            if inner.arena[*succ].data.active_cnt == 0 {
                queue.push_back(*succ);
            }
        }

        if pivot_changed {
            if inner.pivot_chain.len() > EPOCH_SET_PERSISTENCE_DELAY as usize {
                let fork_at_pivot_index = inner.height_to_pivot_index(fork_at);
                // Starting from old_len ensures that all epochs within
                // [old_len - delay, new_len - delay) will be inserted to db, so
                // no epochs will be skipped. Starting from
                // fork_at ensures that any epoch set change will be
                // overwritten.
                let start_pivot_index = if old_pivot_chain_len
                    >= EPOCH_SET_PERSISTENCE_DELAY as usize
                {
                    min(
                        fork_at_pivot_index,
                        old_pivot_chain_len
                            - EPOCH_SET_PERSISTENCE_DELAY as usize,
                    )
                } else {
                    fork_at_pivot_index
                };
                let to_persist_pivot_index = inner.pivot_chain.len()
                    - EPOCH_SET_PERSISTENCE_DELAY as usize;
                for pivot_index in start_pivot_index..to_persist_pivot_index {
                    inner.persist_epoch_set_hashes(pivot_index);
                }
            }
        }

        inner.adjust_difficulty(*inner.pivot_chain.last().expect("not empty"));
        meter.update_confirmation_risks(inner);

        // Note that after the checkpoint (if happens), the old_pivot_chain_len
        // value will become obsolete
        let old_pivot_chain_height =
            inner.pivot_index_to_height(old_pivot_chain_len);
        let new_pivot_era_block = inner.get_era_genesis_block_with_parent(
            *inner.pivot_chain.last().unwrap(),
            0,
        );

        inner.cur_era_stable_height = self.should_move_stable_height(inner);

        let new_era_height = inner.arena[new_pivot_era_block].height;
        let new_checkpoint_era_genesis = self.should_form_checkpoint_at(inner);
        if new_checkpoint_era_genesis != inner.cur_era_genesis_block_arena_index
        {
            info!(
                "Working on new checkpoint, old checkpoint block {} height {}",
                &inner.arena[inner.cur_era_genesis_block_arena_index].hash,
                inner.cur_era_genesis_height
            );

            ConsensusNewBlockHandler::make_checkpoint_at(
                inner,
                new_checkpoint_era_genesis,
                has_transactions,
                &self.executor,
            );
            let stable_era_genesis_arena_index =
                inner.ancestor_at(me, inner.cur_era_stable_height);
            meter.reset_for_checkpoint(
                inner.weight_tree.get(stable_era_genesis_arena_index),
                inner.cur_era_stable_height,
            );
            meter.update_confirmation_risks(inner);
            info!(
                "New checkpoint formed at block {} stable block {} height {}",
                &inner.arena[inner.cur_era_genesis_block_arena_index].hash,
                &inner.arena[stable_era_genesis_arena_index].hash,
                inner.cur_era_genesis_height
            );
        }
        // FIXME: we need a function to compute the deferred epoch
        // FIXME: number. the current codebase may not be
        // FIXME: consistent at all places.
        let mut confirmed_height = meter.get_confirmed_epoch_num(
            inner.cur_era_genesis_height
                + 2 * self.data_man.get_snapshot_epoch_count() as u64
                + DEFERRED_STATE_EPOCH_COUNT,
        );
        if confirmed_height < DEFERRED_STATE_EPOCH_COUNT {
            confirmed_height = 0;
        } else {
            confirmed_height -= DEFERRED_STATE_EPOCH_COUNT;
        }
        // We can not assume that confirmed epoch are already executed,
        // but we can assume that the deferred block are executed.
        let confirmed_epoch_hash = inner
            .get_hash_from_epoch_number(confirmed_height)
            // FIXME: shouldn't unwrap but the function doesn't return error...
            .expect(&concat!(file!(), ":", line!(), ":", column!()));
        // FIXME: we also need more helper function to get the execution result
        // FIXME: for block deferred or not.
        if let Some(confirmed_epoch) = &*self
            .data_man
            .get_epoch_execution_commitment(&confirmed_epoch_hash)
        {
            if confirmed_height
                > self.data_man.state_availability_boundary.read().lower_bound
            {
                // FIXME: how about archive node?
                self.data_man
                    .storage_manager
                    .get_storage_manager()
                    .maintain_snapshots_pivot_chain_confirmed(
                        confirmed_height,
                        &confirmed_epoch_hash,
                        &confirmed_epoch.state_root_with_aux_info,
                        &self.data_man.state_availability_boundary,
                    )
                    // FIXME: propogate error.
                    .expect(&concat!(file!(), ":", line!(), ":", column!()));
            }
        }

        let era_genesis_height =
            inner.get_era_genesis_height(inner.arena[parent].height, 0);
        let cur_pivot_era_block = if inner
            .pivot_index_to_height(inner.pivot_chain.len())
            > era_genesis_height
        {
            inner.get_pivot_block_arena_index(era_genesis_height)
        } else {
            NULL
        };
        let era_block = inner.get_era_genesis_block_with_parent(parent, 0);

        // FIXME: this is header only.
        // If we are inserting header only, we will skip execution and
        // tx_pool-related operations
        if has_transactions {
            // It's only correct to set tx stale after the block is considered
            // terminal for mining.
            // Note that we conservatively only mark those blocks inside the
            // current pivot era
            if era_block == cur_pivot_era_block {
                self.txpool
                    .set_tx_packed(&transactions.expect("Already checked"));
            }
            if new_era_height + ERA_RECYCLE_TRANSACTION_DELAY
                < inner.pivot_index_to_height(inner.pivot_chain.len())
                && inner.last_recycled_era_block != new_pivot_era_block
            {
                self.recycle_tx_outside_era(inner, new_pivot_era_block);
                inner.last_recycled_era_block = new_pivot_era_block;
            }

            let to_state_pos = if inner
                .pivot_index_to_height(inner.pivot_chain.len())
                < DEFERRED_STATE_EPOCH_COUNT
            {
                0
            } else {
                inner.pivot_index_to_height(inner.pivot_chain.len())
                    - DEFERRED_STATE_EPOCH_COUNT
                    + 1
            };
            let mut state_at = fork_at;
            if fork_at + DEFERRED_STATE_EPOCH_COUNT > old_pivot_chain_height {
                if old_pivot_chain_height > DEFERRED_STATE_EPOCH_COUNT {
                    state_at =
                        old_pivot_chain_height - DEFERRED_STATE_EPOCH_COUNT + 1;
                } else {
                    state_at = 1;
                }
            }
            {
                let mut state_availability_boundary =
                    inner.data_man.state_availability_boundary.write();
                assert!(
                    fork_at > state_availability_boundary.lower_bound,
                    "forked_at {} should > boundary_lower_bound, boundary {:?}",
                    fork_at,
                    state_availability_boundary
                );
                if pivot_changed {
                    if extend_pivot {
                        state_availability_boundary
                            .pivot_chain
                            .push(inner.arena[me].hash);
                    } else {
                        let split_off_index =
                            fork_at - state_availability_boundary.lower_bound;
                        state_availability_boundary
                            .pivot_chain
                            .split_off(split_off_index as usize);
                        for i in inner.height_to_pivot_index(fork_at)
                            ..inner.pivot_chain.len()
                        {
                            state_availability_boundary
                                .pivot_chain
                                .push(inner.arena[inner.pivot_chain[i]].hash);
                        }
                        if state_availability_boundary.upper_bound >= fork_at {
                            state_availability_boundary.upper_bound =
                                fork_at - 1;
                        }
                    }
                    state_availability_boundary.optimistic_executed_height =
                        if to_state_pos > 0 {
                            Some(to_state_pos)
                        } else {
                            None
                        };
                }
                // For full node, we don't execute blocks before available
                // states. This skip should only happen in
                // `SyncBlockPhase` for full nodes
                if state_at < state_availability_boundary.lower_bound + 1 {
                    state_at = state_availability_boundary.lower_bound + 1;
                }
            }

            // Apply transactions in the determined total order
            while state_at < to_state_pos {
                let epoch_arena_index =
                    inner.get_pivot_block_arena_index(state_at);
                let reward_execution_info = self
                    .executor
                    .get_reward_execution_info(inner, epoch_arena_index);
                self.executor.enqueue_epoch(EpochExecutionTask::new(
                    inner.arena[epoch_arena_index].hash,
                    inner.get_epoch_block_hashes(epoch_arena_index),
                    inner.get_epoch_start_block_number(epoch_arena_index),
                    reward_execution_info,
                    true,
                    false,
                ));
                state_at += 1;
            }
        }

        let block_status = if inner.arena[me].data.pending {
            BlockStatus::Pending
        } else if inner.arena[me].data.partial_invalid {
            BlockStatus::PartialInvalid
        } else {
            BlockStatus::Valid
        };
        self.persist_terminal_and_block_info(
            inner,
            me,
            block_status,
            has_transactions,
        );
        debug!(
            "Finish activating block in ConsensusGraph: index={:?} hash={:?}",
            me, inner.arena[me].hash
        );
    }

    /// The top level function invoked by ConsensusGraph to insert a new block.
    pub fn on_new_block(
        &self, inner: &mut ConsensusGraphInner, meter: &ConfirmationMeter,
        hash: &H256, block_header: &BlockHeader,
        transactions: Option<Vec<Arc<SignedTransaction>>>,
    )
    {
        let parent_hash = block_header.parent_hash();
        let parent_index = inner.hash_to_arena_indices.get(&parent_hash);
        // current block is outside era or it's parent is outside era
        if parent_index.is_none()
            || inner.arena[*parent_index.unwrap()].era_block == NULL
        {
            debug!(
                "parent={:?} not in consensus graph, set header to pending",
                parent_hash
            );
            let sn = self.process_outside_block(inner, &block_header);
            let block_status_in_db = self
                .data_man
                .local_block_info_from_db(hash)
                .map(|info| info.get_status())
                .unwrap_or(BlockStatus::Pending);
            let block_info = LocalBlockInfo::new(
                block_status_in_db,
                sn,
                self.data_man.get_instance_id(),
            );
            self.data_man
                .insert_local_block_info_to_db(hash, block_info);
            return;
        }

        let me = self.insert_block_initial(inner, &block_header);
        inner.transaction_caches.insert(me, transactions);

        if inner.arena[me].data.active_cnt == 0 {
            let mut queue: VecDeque<usize> = VecDeque::new();
            queue.push_back(me);
            while let Some(me) = queue.pop_front() {
                let block_status = self.preactivate_block(inner, me);

                if block_status == BlockStatus::PartialInvalid {
                    inner.arena[me].data.partial_invalid = true;
                    let last_index =
                        inner.arena[me].last_timer_block_arena_index;
                    let timer = if last_index == NULL {
                        inner.inner_conf.timer_chain_beta
                    } else {
                        inner.arena[last_index].timer_chain_height
                            + inner.inner_conf.timer_chain_beta
                            + if inner.arena[last_index].is_timer
                                && !inner.arena[last_index].data.partial_invalid
                            {
                                1
                            } else {
                                0
                            }
                    };
                    // We are not going to delay partial invalid blocks in the
                    // bench mode
                    if self.conf.bench_mode {
                        inner.invalid_block_queue.push((0, me));
                    } else {
                        inner.invalid_block_queue.push((-(timer as i128), me));
                    }
                    inner.arena[me].data.active_cnt = NULL;
                    debug!(
                        "Block {} (hash = {}) is partially invalid, all of its future will be non-active till timer height {}",
                        me, inner.arena[me].hash, timer
                    );
                } else {
                    if block_status == BlockStatus::Pending {
                        inner.arena[me].data.pending = true;
                        //
                        // ConsensusNewBlockHandler::
                        // try_clear_blockset_in_own_view_of_epoch(
                        //                            inner, me,
                        //                        );
                        debug!(
                            "Block {} (hash = {}) is pending but processed",
                            me, inner.arena[me].hash
                        );
                    } else {
                        debug!(
                            "Block {} (hash = {}) is fully valid",
                            me, inner.arena[me].hash
                        );
                    }
                    let transactions =
                        inner.transaction_caches.remove(&me).unwrap();
                    self.activate_block(
                        inner,
                        me,
                        meter,
                        transactions,
                        &mut queue,
                    );
                }
                // Now we are going to check all invalid blocks in the delay
                // queue Activate them if the timer is
                // up
                let timer = if let Some(x) = inner.timer_chain.last() {
                    inner.arena[*x].timer_chain_height
                } else {
                    inner.cur_era_genesis_timer_chain_height
                };
                loop {
                    if let Some((t, _)) = inner.invalid_block_queue.peek() {
                        if timer < (-*t) as u64 {
                            break;
                        }
                    } else {
                        break;
                    }
                    let (_, x) = inner.invalid_block_queue.pop().unwrap();
                    assert!(inner.arena[me].data.active_cnt == NULL);
                    inner.arena[me].data.active_cnt = 0;
                    let transactions =
                        inner.transaction_caches.remove(&me).unwrap();
                    self.activate_block(
                        inner,
                        x,
                        meter,
                        transactions,
                        &mut queue,
                    );
                }
            }
        } else {
            debug!(
                "Block {} (hash = {}) is non-active with active counter {}",
                me, inner.arena[me].hash, inner.arena[me].data.active_cnt
            );
        }
    }

    fn persist_terminal_and_block_info(
        &self, inner: &mut ConsensusGraphInner, me: usize,
        block_status: BlockStatus, persist_terminal: bool,
    )
    {
        if persist_terminal {
            self.persist_terminals(inner);
        }

        let block_info = LocalBlockInfo::new(
            block_status,
            inner.arena[me].data.sequence_number,
            self.data_man.get_instance_id(),
        );
        self.data_man
            .insert_local_block_info_to_db(&inner.arena[me].hash, block_info);
    }

    /// construct_pivot_state() rebuild pivot chain state info from db
    /// avoiding intermediate redundant computation triggered by
    /// on_new_block().
    /// It also recovers receipts_root and logs_bloom_hash in pivot chain.
    /// This function is only invoked from recover_graph_from_db with
    /// header_only being false.
    pub fn construct_pivot_state(&self, inner: &mut ConsensusGraphInner) {
        // FIXME: this line doesn't exactly match its purpose.
        // FIXME: Is it the checkpoint or synced snapshot or could it be
        // anything else?
        let state_boundary_height =
            self.data_man.state_availability_boundary.read().lower_bound;
        let start_pivot_index =
            (state_boundary_height - inner.cur_era_genesis_height) as usize;
        let start_hash = inner.arena[inner.pivot_chain[start_pivot_index]].hash;
        // Here, we should ensure the epoch_execution_commitment for stable hash
        // must be loaded into memory. Since, in some rare cases, the number of
        // blocks between stable and best_epoch is less than
        // DEFERRED_STATE_EPOCH_COUNT, the for loop below will not load
        // epoch_execution_commitment for stable hash.
        if start_hash != inner.data_man.true_genesis.hash()
            && self
                .data_man
                .get_epoch_execution_commitment(&start_hash)
                .is_none()
        {
            self.data_man.load_epoch_execution_commitment_from_db(&start_hash)
                .expect("epoch_execution_commitment for stable hash must exist in disk");
        }
        {
            let mut state_availability_boundary =
                self.data_man.state_availability_boundary.write();
            assert!(
                state_availability_boundary.lower_bound
                    == state_availability_boundary.upper_bound
            );
            for pivot_index in start_pivot_index + 1..inner.pivot_chain.len() {
                state_availability_boundary
                    .pivot_chain
                    .push(inner.arena[inner.pivot_chain[pivot_index]].hash);
            }
        }

        if inner.pivot_chain.len() < DEFERRED_STATE_EPOCH_COUNT as usize {
            return;
        }
        for pivot_index in start_pivot_index + 1
            ..inner.pivot_chain.len() - DEFERRED_STATE_EPOCH_COUNT as usize + 1
        {
            let arena_index = inner.pivot_chain[pivot_index];
            let pivot_hash = inner.arena[arena_index].hash;

            // Ensure that the commitments for the blocks on
            // pivot_chain after cur_era_stable_genesis are kept in memory.
            if self
                .data_man
                .load_epoch_execution_commitment_from_db(&pivot_hash)
                .is_none()
            {
                // We should recompute the epochs that should have been executed
                // but fail to persist their
                // execution_commitments before shutdown
                let reward_execution_info =
                    self.executor.get_reward_execution_info(inner, arena_index);
                let epoch_block_hashes =
                    inner.get_epoch_block_hashes(arena_index);
                let start_block_number =
                    inner.get_epoch_start_block_number(arena_index);
                self.executor.compute_epoch(EpochExecutionTask::new(
                    pivot_hash,
                    epoch_block_hashes,
                    start_block_number,
                    reward_execution_info,
                    true,
                    false,
                ));
            } else {
                self.data_man
                    .state_availability_boundary
                    .write()
                    .upper_bound += 1;
            }
        }
    }
}
