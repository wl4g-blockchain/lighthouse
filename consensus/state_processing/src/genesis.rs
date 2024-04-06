use super::per_block_processing::{
    errors::BlockProcessingError, process_operations::process_deposit,
};
use crate::common::DepositDataTree;
use crate::per_block_processing::UNSET_DEPOSIT_RECEIPTS_START_INDEX;
use crate::upgrade::{
    upgrade_to_altair, upgrade_to_bellatrix, upgrade_to_capella, upgrade_to_deneb,
    upgrade_to_electra,
};
use safe_arith::{ArithError, SafeArith};
use tree_hash::TreeHash;
use types::*;

/// Initialize a `BeaconState` from genesis data.
pub fn initialize_beacon_state_from_eth1<E: EthSpec>(
    eth1_block_hash: Hash256,
    eth1_timestamp: u64,
    deposits: Vec<Deposit>,
    execution_payload_header: Option<ExecutionPayloadHeader<E>>,
    spec: &ChainSpec,
) -> Result<BeaconState<E>, BlockProcessingError> {
    let genesis_time = eth2_genesis_time(eth1_timestamp, spec)?;
    let eth1_data = Eth1Data {
        deposit_root: Hash256::zero(),
        deposit_count: deposits.len() as u64,
        block_hash: eth1_block_hash,
    };
    let mut state = BeaconState::new(genesis_time, eth1_data, spec);

    // Seed RANDAO with Eth1 entropy
    state.fill_randao_mixes_with(eth1_block_hash);

    let mut deposit_tree = DepositDataTree::create(&[], 0, DEPOSIT_TREE_DEPTH);

    for deposit in deposits.iter() {
        deposit_tree
            .push_leaf(deposit.data.tree_hash_root())
            .map_err(BlockProcessingError::MerkleTreeError)?;
        state.eth1_data_mut().deposit_root = deposit_tree.root();
        process_deposit(&mut state, deposit, spec, true)?;
    }

    process_activations(&mut state, spec)?;

    // Set genesis validators root for domain separation and chain versioning
    *state.genesis_validators_root_mut() = state.update_validators_tree_hash_cache()?;

    // Add deposit_receipts_start_index field with the value UNSET_DEPOSIT_RECEIPTS_START_INDEX
    *state.deposit_receipts_start_index_mut()? = UNSET_DEPOSIT_RECEIPTS_START_INDEX;

    if spec
        .altair_fork_epoch
        .map_or(false, |fork_epoch| fork_epoch == E::genesis_epoch())
    {
        upgrade_to_altair(&mut state, spec)?;

        state.fork_mut().previous_version = spec.altair_fork_version;
    }

    // Similarly, perform an upgrade to the merge if configured from genesis.
    if spec
        .bellatrix_fork_epoch
        .map_or(false, |fork_epoch| fork_epoch == E::genesis_epoch())
    {
        // this will set state.latest_execution_payload_header = ExecutionPayloadHeaderMerge::default()
        upgrade_to_bellatrix(&mut state, spec)?;

        // Remove intermediate Altair fork from `state.fork`.
        state.fork_mut().previous_version = spec.bellatrix_fork_version;

        // Override latest execution payload header.
        // See https://github.com/ethereum/consensus-specs/blob/v1.1.0/specs/bellatrix/beacon-chain.md#testing
        if let Some(ExecutionPayloadHeader::Merge(ref header)) = execution_payload_header {
            *state.latest_execution_payload_header_merge_mut()? = header.clone();
        }
    }

    // Upgrade to capella if configured from genesis
    if spec
        .capella_fork_epoch
        .map_or(false, |fork_epoch| fork_epoch == E::genesis_epoch())
    {
        upgrade_to_capella(&mut state, spec)?;

        // Remove intermediate Bellatrix fork from `state.fork`.
        state.fork_mut().previous_version = spec.capella_fork_version;

        // Override latest execution payload header.
        // See https://github.com/ethereum/consensus-specs/blob/dev/specs/capella/beacon-chain.md#testing
        if let Some(ExecutionPayloadHeader::Capella(ref header)) = execution_payload_header {
            *state.latest_execution_payload_header_capella_mut()? = header.clone();
        }
    }

    // Upgrade to deneb if configured from genesis
    if spec
        .deneb_fork_epoch
        .map_or(false, |fork_epoch| fork_epoch == E::genesis_epoch())
    {
        upgrade_to_deneb(&mut state, spec)?;

        // Remove intermediate Capella fork from `state.fork`.
        state.fork_mut().previous_version = spec.deneb_fork_version;

        // Override latest execution payload header.
        // See https://github.com/ethereum/consensus-specs/blob/dev/specs/deneb/beacon-chain.md#testing
        if let Some(ExecutionPayloadHeader::Deneb(ref header)) = execution_payload_header {
            *state.latest_execution_payload_header_deneb_mut()? = header.clone();
        }
    }

    // Upgrade to electra if configured from genesis.
    if spec
        .electra_fork_epoch
        .map_or(false, |fork_epoch| fork_epoch == E::genesis_epoch())
    {
        upgrade_to_electra(&mut state, spec)?;

        // Remove intermediate Deneb fork from `state.fork`.
        state.fork_mut().previous_version = spec.electra_fork_version;

        // Override latest execution payload header.
        // See https://github.com/ethereum/consensus-specs/blob/dev/specs/capella/beacon-chain.md#testing
        if let Some(ExecutionPayloadHeader::Electra(header)) = execution_payload_header {
            *state.latest_execution_payload_header_electra_mut()? = header.clone();
        }
    }

    // Now that we have our validators, initialize the caches (including the committees)
    state.build_caches(spec)?;

    Ok(state)
}

/// Determine whether a candidate genesis state is suitable for starting the chain.
pub fn is_valid_genesis_state<E: EthSpec>(state: &BeaconState<E>, spec: &ChainSpec) -> bool {
    state
        .get_active_validator_indices(E::genesis_epoch(), spec)
        .map_or(false, |active_validators| {
            state.genesis_time() >= spec.min_genesis_time
                && active_validators.len() as u64 >= spec.min_genesis_active_validator_count
        })
}

/// Activate genesis validators, if their balance is acceptable.
pub fn process_activations<E: EthSpec>(
    state: &mut BeaconState<E>,
    spec: &ChainSpec,
) -> Result<(), Error> {
    let (validators, balances, _) = state.validators_and_balances_and_progressive_balances_mut();
    for (index, validator) in validators.iter_mut().enumerate() {
        let balance = balances
            .get(index)
            .copied()
            .ok_or(Error::BalancesOutOfBounds(index))?;
        validator.effective_balance = std::cmp::min(
            balance.safe_sub(balance.safe_rem(spec.effective_balance_increment)?)?,
            spec.max_effective_balance,
        );
        if validator.effective_balance == spec.max_effective_balance {
            validator.activation_eligibility_epoch = E::genesis_epoch();
            validator.activation_epoch = E::genesis_epoch();
        }
    }
    Ok(())
}

/// Returns the `state.genesis_time` for the corresponding `eth1_timestamp`.
///
/// Does _not_ ensure that the time is greater than `MIN_GENESIS_TIME`.
///
/// Spec v0.12.1
pub fn eth2_genesis_time(eth1_timestamp: u64, spec: &ChainSpec) -> Result<u64, ArithError> {
    eth1_timestamp.safe_add(spec.genesis_delay)
}
