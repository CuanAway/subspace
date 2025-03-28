// Copyright (C) 2021 Subspace Labs, Inc.
// SPDX-License-Identifier: GPL-3.0-or-later

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

#![cfg_attr(not(feature = "std"), no_std)]
#![feature(const_option)]
// `construct_runtime!` does a lot of recursion and requires us to increase the limit to 256.
#![recursion_limit = "256"]

// Make execution WASM runtime available.
include!(concat!(env!("OUT_DIR"), "/execution_wasm_bundle.rs"));

// Make the WASM binary available.
#[cfg(feature = "std")]
include!(concat!(env!("OUT_DIR"), "/wasm_binary.rs"));

use codec::{Compact, CompactLen, Decode, Encode};
use core::time::Duration;
use frame_support::traits::{
    ConstU128, ConstU16, ConstU32, ConstU64, ConstU8, Currency, ExistenceRequirement, Get,
    Imbalance, WithdrawReasons,
};
use frame_support::weights::constants::{RocksDbWeight, WEIGHT_PER_SECOND};
use frame_support::weights::{ConstantMultiplier, IdentityFee};
use frame_support::{construct_runtime, parameter_types};
use frame_system::limits::{BlockLength, BlockWeights};
use frame_system::EnsureNever;
use pallet_balances::NegativeImbalance;
use pallet_feeds::feed_processor::{FeedMetadata, FeedObjectMapping, FeedProcessor};
use pallet_grandpa_finality_verifier::chain::Chain;
use sp_api::{impl_runtime_apis, BlockT, HashT, HeaderT};
use sp_consensus_subspace::digests::CompatibleDigestItem;
use sp_consensus_subspace::{
    derive_randomness, EquivocationProof, FarmerPublicKey, GlobalRandomnesses, Salts, SignedVote,
    SolutionRanges, Vote,
};
use sp_core::crypto::{ByteArray, KeyTypeId};
use sp_core::{Hasher, OpaqueMetadata};
use sp_executor::{FraudProof, OpaqueBundle};
use sp_runtime::traits::{
    AccountIdLookup, BlakeTwo256, DispatchInfoOf, NumberFor, PostDispatchInfoOf, Zero,
};
use sp_runtime::transaction_validity::{
    InvalidTransaction, TransactionSource, TransactionValidity, TransactionValidityError,
};
use sp_runtime::{
    create_runtime_str, generic, AccountId32, ApplyExtrinsicResult, DispatchError, OpaqueExtrinsic,
    Perbill,
};
use sp_std::borrow::Cow;
use sp_std::iter::Peekable;
use sp_std::marker::PhantomData;
use sp_std::prelude::*;
#[cfg(feature = "std")]
use sp_version::NativeVersion;
use sp_version::RuntimeVersion;
use subspace_core_primitives::objects::{BlockObject, BlockObjectMapping};
use subspace_core_primitives::{Randomness, RootBlock, Sha256Hash, PIECE_SIZE};
use subspace_runtime_primitives::{
    opaque, AccountId, Balance, BlockNumber, Hash, Index, Moment, Signature, CONFIRMATION_DEPTH_K,
    MAX_PLOT_SIZE, MIN_REPLICATION_FACTOR, RECORDED_HISTORY_SEGMENT_SIZE, RECORD_SIZE,
    STORAGE_FEES_ESCROW_BLOCK_REWARD, STORAGE_FEES_ESCROW_BLOCK_TAX,
};

sp_runtime::impl_opaque_keys! {
    pub struct SessionKeys {
    }
}

// To learn more about runtime versioning and what each of the following value means:
//   https://substrate.dev/docs/en/knowledgebase/runtime/upgrades#runtime-versioning
#[sp_version::runtime_version]
pub const VERSION: RuntimeVersion = RuntimeVersion {
    spec_name: create_runtime_str!("subspace"),
    impl_name: create_runtime_str!("subspace"),
    authoring_version: 1,
    // The version of the runtime specification. A full node will not attempt to use its native
    //   runtime in substitute for the on-chain Wasm runtime unless all of `spec_name`,
    //   `spec_version`, and `authoring_version` are the same between Wasm and native.
    // This value is set to 100 to notify Polkadot-JS App (https://polkadot.js.org/apps) to use
    //   the compatible custom types.
    spec_version: 100,
    impl_version: 1,
    apis: RUNTIME_API_VERSIONS,
    transaction_version: 1,
    state_version: 1,
};

/// The version information used to identify this runtime when compiled natively.
#[cfg(feature = "std")]
pub fn native_version() -> NativeVersion {
    NativeVersion {
        runtime_version: VERSION,
        can_author_with: Default::default(),
    }
}

/// The smallest unit of the token is called Shannon.
pub const SHANNON: Balance = 1;
/// Subspace Credits have 18 decimal places.
pub const DECIMAL_PLACES: u8 = 18;
/// One Subspace Credit.
pub const SSC: Balance = (10 * SHANNON).pow(DECIMAL_PLACES as u32);

// TODO: Many of below constants should probably be updatable but currently they are not

/// Since Subspace is probabilistic this is the average expected block time that
/// we are targeting. Blocks will be produced at a minimum duration defined
/// by `SLOT_DURATION`, but some slots will not be allocated to any
/// farmer and hence no block will be produced. We expect to have this
/// block time on average following the defined slot duration and the value
/// of `c` configured for Subspace (where `1 - c` represents the probability of
/// a slot being empty).
/// This value is only used indirectly to define the unit constants below
/// that are expressed in blocks. The rest of the code should use
/// `SLOT_DURATION` instead (like the Timestamp pallet for calculating the
/// minimum period).
///
/// Based on:
/// <https://research.web3.foundation/en/latest/polkadot/block-production/Babe.html#-6.-practical-results>
pub const MILLISECS_PER_BLOCK: u64 = 6000;

// NOTE: Currently it is not possible to change the slot duration after the chain has started.
//       Attempting to do so will brick block production.
const SLOT_DURATION: u64 = 1000;

/// 1 in 6 slots (on average, not counting collisions) will have a block.
/// Must match ratio between block and slot duration in constants above.
const SLOT_PROBABILITY: (u64, u64) = (1, 6);

/// The amount of time, in blocks, between updates of global randomness.
const GLOBAL_RANDOMNESS_UPDATE_INTERVAL: BlockNumber = 100;

/// Era duration in blocks.
const ERA_DURATION_IN_BLOCKS: BlockNumber = 2016;

const EQUIVOCATION_REPORT_LONGEVITY: BlockNumber = 256;

/// Eon duration is 7 days
const EON_DURATION_IN_SLOTS: u64 = 3600 * 24 * 7;
/// Reveal next eon salt 1 day before eon end
const EON_NEXT_SALT_REVEAL: u64 = EON_DURATION_IN_SLOTS
    .checked_sub(3600 * 24)
    .expect("Offset is smaller than eon duration; qed");

/// Any solution range is valid in the test environment.
const INITIAL_SOLUTION_RANGE: u64 = u64::MAX;

/// A ratio of `Normal` dispatch class within block, for `BlockWeight` and `BlockLength`.
const NORMAL_DISPATCH_RATIO: Perbill = Perbill::from_percent(75);

/// Maximum block length for non-`Normal` extrinsic is 5 MiB.
const MAX_BLOCK_LENGTH: u32 = 5 * 1024 * 1024;

const MAX_OBJECT_MAPPING_RECURSION_DEPTH: u16 = 5;

parameter_types! {
    pub const Version: RuntimeVersion = VERSION;
    pub const BlockHashCount: BlockNumber = 2400;
    /// We allow for 2 seconds of compute with a 6 second average block time.
    pub SubspaceBlockWeights: BlockWeights = BlockWeights::with_sensible_defaults(2 * WEIGHT_PER_SECOND, NORMAL_DISPATCH_RATIO);
    /// We allow for 3.75 MiB for `Normal` extrinsic with 5 MiB maximum block length.
    pub SubspaceBlockLength: BlockLength = BlockLength::max_with_normal_ratio(MAX_BLOCK_LENGTH, NORMAL_DISPATCH_RATIO);
}

pub type SS58Prefix = ConstU16<2254>;

// Configure FRAME pallets to include in runtime.

impl frame_system::Config for Runtime {
    /// The basic call filter to use in dispatchable.
    type BaseCallFilter = frame_support::traits::Everything;
    /// Block & extrinsics weights: base values and limits.
    type BlockWeights = SubspaceBlockWeights;
    /// The maximum length of a block (in bytes).
    type BlockLength = SubspaceBlockLength;
    /// The identifier used to distinguish between accounts.
    type AccountId = AccountId;
    /// The aggregated dispatch type that is available for extrinsics.
    type Call = Call;
    /// The lookup mechanism to get account ID from whatever is passed in dispatchers.
    type Lookup = AccountIdLookup<AccountId, ()>;
    /// The index type for storing how many extrinsics an account has signed.
    type Index = Index;
    /// The index type for blocks.
    type BlockNumber = BlockNumber;
    /// The type for hashing blocks and tries.
    type Hash = Hash;
    /// The hashing algorithm used.
    type Hashing = BlakeTwo256;
    /// The header type.
    type Header = Header;
    /// The ubiquitous event type.
    type Event = Event;
    /// The ubiquitous origin type.
    type Origin = Origin;
    /// Maximum number of block number to block hash mappings to keep (oldest pruned first).
    type BlockHashCount = ConstU32<250>;
    /// The weight of database operations that the runtime can invoke.
    type DbWeight = RocksDbWeight;
    /// Version of the runtime.
    type Version = Version;
    /// Converts a module to the index of the module in `construct_runtime!`.
    ///
    /// This type is being generated by `construct_runtime!`.
    type PalletInfo = PalletInfo;
    /// What to do if a new account is created.
    type OnNewAccount = ();
    /// What to do if an account is fully reaped from the system.
    type OnKilledAccount = ();
    /// The data to be stored in an account.
    type AccountData = pallet_balances::AccountData<Balance>;
    /// Weight information for the extrinsics of this pallet.
    type SystemWeightInfo = ();
    /// This is used as an identifier of the chain.
    type SS58Prefix = SS58Prefix;
    /// The set code logic, just the default since we're not a parachain.
    type OnSetCode = ();
    type MaxConsumers = ConstU32<16>;
}

parameter_types! {
    pub const SlotProbability: (u64, u64) = SLOT_PROBABILITY;
    pub const ExpectedBlockTime: Moment = MILLISECS_PER_BLOCK;
    pub const ShouldAdjustSolutionRange: bool = false;
    pub const ExpectedVotesPerBlock: u32 = 9;
}

impl pallet_subspace::Config for Runtime {
    type Event = Event;
    type GlobalRandomnessUpdateInterval = ConstU32<GLOBAL_RANDOMNESS_UPDATE_INTERVAL>;
    type EraDuration = ConstU32<ERA_DURATION_IN_BLOCKS>;
    type EonDuration = ConstU64<EON_DURATION_IN_SLOTS>;
    type EonNextSaltReveal = ConstU64<EON_NEXT_SALT_REVEAL>;
    type InitialSolutionRange = ConstU64<INITIAL_SOLUTION_RANGE>;
    type SlotProbability = SlotProbability;
    type ExpectedBlockTime = ExpectedBlockTime;
    type ConfirmationDepthK = ConstU32<CONFIRMATION_DEPTH_K>;
    type RecordSize = ConstU32<RECORD_SIZE>;
    type MaxPlotSize = ConstU64<MAX_PLOT_SIZE>;
    type RecordedHistorySegmentSize = ConstU32<RECORDED_HISTORY_SEGMENT_SIZE>;
    type ExpectedVotesPerBlock = ExpectedVotesPerBlock;
    type ShouldAdjustSolutionRange = ShouldAdjustSolutionRange;
    type GlobalRandomnessIntervalTrigger = pallet_subspace::NormalGlobalRandomnessInterval;
    type EraChangeTrigger = pallet_subspace::NormalEraChange;
    type EonChangeTrigger = pallet_subspace::NormalEonChange;

    type HandleEquivocation = pallet_subspace::equivocation::EquivocationHandler<
        OffencesSubspace,
        ConstU64<{ EQUIVOCATION_REPORT_LONGEVITY as u64 }>,
    >;

    type WeightInfo = ();
}

impl pallet_timestamp::Config for Runtime {
    /// A timestamp: milliseconds since the unix epoch.
    type Moment = Moment;
    type OnTimestampSet = Subspace;
    type MinimumPeriod = ConstU64<{ SLOT_DURATION / 2 }>;
    type WeightInfo = ();
}

impl pallet_balances::Config for Runtime {
    type MaxLocks = ConstU32<50>;
    type MaxReserves = ();
    type ReserveIdentifier = [u8; 8];
    /// The type for recording an account's balance.
    type Balance = Balance;
    /// The ubiquitous event type.
    type Event = Event;
    type DustRemoval = ();
    // TODO: Correct value
    type ExistentialDeposit = ConstU128<{ 500 * SHANNON }>;
    type AccountStore = System;
    type WeightInfo = pallet_balances::weights::SubstrateWeight<Runtime>;
}

parameter_types! {
    pub const StorageFeesEscrowBlockReward: (u64, u64) = STORAGE_FEES_ESCROW_BLOCK_REWARD;
    pub const StorageFeesEscrowBlockTax: (u64, u64) = STORAGE_FEES_ESCROW_BLOCK_TAX;
}

pub struct CreditSupply;

impl Get<Balance> for CreditSupply {
    fn get() -> Balance {
        Balances::total_issuance()
    }
}

pub struct TotalSpacePledged;

impl Get<u128> for TotalSpacePledged {
    fn get() -> u128 {
        let piece_size = u128::try_from(PIECE_SIZE)
            .expect("Piece size is definitely small enough to fit into u128; qed");
        // Operations reordered to avoid data loss, but essentially are:
        // u64::MAX * SlotProbability / (solution_range / PIECE_SIZE)
        u128::from(u64::MAX)
            .saturating_mul(piece_size)
            .saturating_mul(u128::from(SlotProbability::get().0))
            / u128::from(Subspace::solution_ranges().current)
            / u128::from(SlotProbability::get().1)
    }
}

pub struct BlockchainHistorySize;

impl Get<u128> for BlockchainHistorySize {
    fn get() -> u128 {
        u128::from(Subspace::archived_history_size())
    }
}

impl pallet_transaction_fees::Config for Runtime {
    type Event = Event;
    type MinReplicationFactor = ConstU16<MIN_REPLICATION_FACTOR>;
    type StorageFeesEscrowBlockReward = StorageFeesEscrowBlockReward;
    type StorageFeesEscrowBlockTax = StorageFeesEscrowBlockTax;
    type CreditSupply = CreditSupply;
    type TotalSpacePledged = TotalSpacePledged;
    type BlockchainHistorySize = BlockchainHistorySize;
    type Currency = Balances;
    type FindBlockRewardAddress = Subspace;
    type WeightInfo = ();
}

pub struct TransactionByteFee;

impl Get<Balance> for TransactionByteFee {
    fn get() -> Balance {
        if cfg!(feature = "do-not-enforce-cost-of-storage") {
            1
        } else {
            TransactionFees::transaction_byte_fee()
        }
    }
}

pub struct LiquidityInfo {
    storage_fee: Balance,
    imbalance: NegativeImbalance<Runtime>,
}

/// Implementation of [`pallet_transaction_payment::OnChargeTransaction`] that charges transaction
/// fees and distributes storage/compute fees and tip separately.
pub struct OnChargeTransaction;

impl pallet_transaction_payment::OnChargeTransaction<Runtime> for OnChargeTransaction {
    type LiquidityInfo = Option<LiquidityInfo>;
    type Balance = Balance;

    fn withdraw_fee(
        who: &AccountId,
        call: &Call,
        _info: &DispatchInfoOf<Call>,
        fee: Self::Balance,
        tip: Self::Balance,
    ) -> Result<Self::LiquidityInfo, TransactionValidityError> {
        if fee.is_zero() {
            return Ok(None);
        }

        let withdraw_reason = if tip.is_zero() {
            WithdrawReasons::TRANSACTION_PAYMENT
        } else {
            WithdrawReasons::TRANSACTION_PAYMENT | WithdrawReasons::TIP
        };

        let withdraw_result = <Balances as Currency<AccountId>>::withdraw(
            who,
            fee,
            withdraw_reason,
            ExistenceRequirement::KeepAlive,
        );
        let imbalance = withdraw_result.map_err(|_error| InvalidTransaction::Payment)?;

        // Separate storage fee while we have access to the call data structure to calculate it.
        let storage_fee = TransactionByteFee::get()
            * Balance::try_from(call.encoded_size())
                .expect("Size of the call never exceeds balance units; qed");

        Ok(Some(LiquidityInfo {
            storage_fee,
            imbalance,
        }))
    }

    fn correct_and_deposit_fee(
        who: &AccountId,
        _dispatch_info: &DispatchInfoOf<Call>,
        _post_info: &PostDispatchInfoOf<Call>,
        corrected_fee: Self::Balance,
        tip: Self::Balance,
        liquidity_info: Self::LiquidityInfo,
    ) -> Result<(), TransactionValidityError> {
        if let Some(LiquidityInfo {
            storage_fee,
            imbalance,
        }) = liquidity_info
        {
            // Calculate how much refund we should return
            let refund_amount = imbalance.peek().saturating_sub(corrected_fee);
            // Refund to the the account that paid the fees. If this fails, the account might have
            // dropped below the existential balance. In that case we don't refund anything.
            let refund_imbalance = Balances::deposit_into_existing(who, refund_amount)
                .unwrap_or_else(|_| <Balances as Currency<AccountId>>::PositiveImbalance::zero());
            // Merge the imbalance caused by paying the fees and refunding parts of it again.
            let adjusted_paid = imbalance
                .offset(refund_imbalance)
                .same()
                .map_err(|_| TransactionValidityError::Invalid(InvalidTransaction::Payment))?;

            // Split the tip from the total fee that ended up being paid.
            let (tip, fee) = adjusted_paid.split(tip);
            // Split paid storage and compute fees so that they can be distributed separately.
            let (paid_storage_fee, paid_compute_fee) = fee.split(storage_fee);

            TransactionFees::note_transaction_fees(
                paid_storage_fee.peek(),
                paid_compute_fee.peek(),
                tip.peek(),
            );
        }
        Ok(())
    }
}

impl pallet_transaction_payment::Config for Runtime {
    type OnChargeTransaction = OnChargeTransaction;
    type OperationalFeeMultiplier = ConstU8<5>;
    type WeightToFee = IdentityFee<Balance>;
    type LengthToFee = ConstantMultiplier<Balance, TransactionByteFee>;
    type FeeMultiplierUpdate = ();
}

impl pallet_utility::Config for Runtime {
    type Event = Event;
    type Call = Call;
    type PalletsOrigin = OriginCaller;
    type WeightInfo = pallet_utility::weights::SubstrateWeight<Runtime>;
}

impl pallet_sudo::Config for Runtime {
    type Event = Event;
    type Call = Call;
}

impl<C> frame_system::offchain::SendTransactionTypes<C> for Runtime
where
    Call: From<C>,
{
    type Extrinsic = UncheckedExtrinsic;
    type OverarchingCall = Call;
}

impl pallet_offences_subspace::Config for Runtime {
    type Event = Event;
    type OnOffenceHandler = Subspace;
}

parameter_types! {
    pub const ReceiptsPruningDepth: BlockNumber = 256;
    pub const MaximumReceiptDrift: BlockNumber = 4;
}

impl pallet_executor::Config for Runtime {
    type Event = Event;
    type SecondaryHash = cirrus_primitives::Hash;
    type ReceiptsPruningDepth = ReceiptsPruningDepth;
    type MaximumReceiptDrift = MaximumReceiptDrift;
}

parameter_types! {
    pub const BlockReward: Balance = SSC / (ExpectedVotesPerBlock::get() as Balance + 1);
    pub const VoteReward: Balance = SSC / (ExpectedVotesPerBlock::get() as Balance + 1);
}

impl pallet_rewards::Config for Runtime {
    type Event = Event;
    type Currency = Balances;
    type BlockReward = BlockReward;
    type VoteReward = VoteReward;
    type FindBlockRewardAddress = Subspace;
    type FindVotingRewardAddresses = Subspace;
    type WeightInfo = ();
}

/// Polkadot-like chain.
struct PolkadotLike;
impl Chain for PolkadotLike {
    type BlockNumber = u32;
    type Hash = <BlakeTwo256 as Hasher>::Out;
    type Header = generic::Header<u32, BlakeTwo256>;
    type Hasher = BlakeTwo256;
}

/// Type used to represent a FeedId or ChainId
pub type FeedId = u64;
pub struct GrandpaValidator<C>(PhantomData<C>);

impl<C: Chain> FeedProcessor<FeedId> for GrandpaValidator<C> {
    fn init(&self, feed_id: FeedId, data: &[u8]) -> sp_runtime::DispatchResult {
        pallet_grandpa_finality_verifier::initialize::<Runtime, C>(feed_id, data)
    }

    fn put(&self, feed_id: FeedId, object: &[u8]) -> Result<Option<FeedMetadata>, DispatchError> {
        Ok(Some(
            pallet_grandpa_finality_verifier::validate_finalized_block::<Runtime, C>(
                feed_id, object,
            )?
            .encode(),
        ))
    }

    fn object_mappings(&self, _feed_id: FeedId, object: &[u8]) -> Vec<FeedObjectMapping> {
        let block = match C::decode_block::<Runtime>(object) {
            Ok(block) => block,
            // we just return empty if we failed to decode as this is not called in runtime
            Err(_) => return vec![],
        };
        // for substrate, we store the height and block hash at that height
        let key = (*block.block.header.number(), block.block.header.hash()).encode();
        vec![FeedObjectMapping::Custom { key, offset: 0 }]
    }

    fn delete(&self, feed_id: FeedId) -> sp_runtime::DispatchResult {
        pallet_grandpa_finality_verifier::purge::<Runtime>(feed_id)
    }
}

parameter_types! {
    pub const MaxFeeds: u32 = 10;
}

impl pallet_feeds::Config for Runtime {
    type Event = Event;
    type FeedId = FeedId;
    type FeedProcessorKind = ();
    type MaxFeeds = MaxFeeds;

    fn feed_processor(
        _feed_processor_id: Self::FeedProcessorKind,
    ) -> Box<dyn FeedProcessor<Self::FeedId>> {
        Box::new(GrandpaValidator(PhantomData::<PolkadotLike>))
    }
}

impl pallet_grandpa_finality_verifier::Config for Runtime {
    type ChainId = FeedId;
}

impl pallet_object_store::Config for Runtime {
    type Event = Event;
}

parameter_types! {
    // This value doesn't matter, we don't use it (`VestedTransferOrigin = EnsureNever` below).
    pub const MinVestedTransfer: Balance = 0;
}

impl orml_vesting::Config for Runtime {
    type Event = Event;
    type Currency = Balances;
    type MinVestedTransfer = MinVestedTransfer;
    type VestedTransferOrigin = EnsureNever<AccountId>;
    type WeightInfo = ();
    type MaxVestingSchedules = ConstU32<2>;
    type BlockNumberProvider = System;
}

construct_runtime!(
    pub enum Runtime where
        Block = Block,
        NodeBlock = opaque::Block,
        UncheckedExtrinsic = UncheckedExtrinsic
    {
        System: frame_system = 0,
        Timestamp: pallet_timestamp = 1,

        Subspace: pallet_subspace = 2,
        OffencesSubspace: pallet_offences_subspace = 3,
        Rewards: pallet_rewards = 9,

        Balances: pallet_balances = 4,
        TransactionFees: pallet_transaction_fees = 12,
        TransactionPayment: pallet_transaction_payment = 5,
        Utility: pallet_utility = 8,

        Feeds: pallet_feeds = 6,
        GrandpaFinalityVerifier: pallet_grandpa_finality_verifier = 13,
        ObjectStore: pallet_object_store = 10,
        Executor: pallet_executor = 11,

        Vesting: orml_vesting = 7,

        // Reserve some room for other pallets as we'll remove sudo pallet eventually.
        Sudo: pallet_sudo = 100,
    }
);

/// The address format for describing accounts.
pub type Address = sp_runtime::MultiAddress<AccountId, ()>;
/// Block header type as expected by this runtime.
pub type Header = generic::Header<BlockNumber, BlakeTwo256>;
/// Block type as expected by this runtime.
pub type Block = generic::Block<Header, UncheckedExtrinsic>;
/// The SignedExtension to the basic transaction logic.
pub type SignedExtra = (
    frame_system::CheckNonZeroSender<Runtime>,
    frame_system::CheckSpecVersion<Runtime>,
    frame_system::CheckTxVersion<Runtime>,
    frame_system::CheckGenesis<Runtime>,
    frame_system::CheckMortality<Runtime>,
    frame_system::CheckNonce<Runtime>,
    frame_system::CheckWeight<Runtime>,
    pallet_transaction_payment::ChargeTransactionPayment<Runtime>,
);
/// Unchecked extrinsic type as expected by this runtime.
pub type UncheckedExtrinsic = generic::UncheckedExtrinsic<Address, Call, Signature, SignedExtra>;
/// Executive: handles dispatch to the various modules.
pub type Executive = frame_executive::Executive<
    Runtime,
    Block,
    frame_system::ChainContext<Runtime>,
    Runtime,
    AllPalletsWithSystem,
>;
/// The payload being signed in transactions.
pub type SignedPayload = generic::SignedPayload<Call, SignedExtra>;

fn extract_root_blocks(ext: &UncheckedExtrinsic) -> Option<Vec<RootBlock>> {
    match &ext.function {
        Call::Subspace(pallet_subspace::Call::store_root_blocks { root_blocks }) => {
            Some(root_blocks.clone())
        }
        _ => None,
    }
}

fn extract_feeds_block_object_mapping<I: Iterator<Item = Hash>>(
    base_offset: u32,
    objects: &mut Vec<BlockObject>,
    call: &pallet_feeds::Call<Runtime>,
    successful_calls: &mut Peekable<I>,
) {
    let call_hash = successful_calls.peek();
    match call_hash {
        Some(hash) => {
            if <BlakeTwo256 as HashT>::hash(call.encode().as_slice()) != *hash {
                return;
            }

            // remove the hash and fetch the object mapping for this call
            successful_calls.next();
        }
        None => return,
    }
    call.extract_call_objects()
        .into_iter()
        .for_each(|object_map| {
            objects.push(BlockObject::V0 {
                hash: object_map.key,
                offset: base_offset + object_map.offset,
            })
        })
}

fn extract_object_store_block_object_mapping(
    base_offset: u32,
    objects: &mut Vec<BlockObject>,
    call: &pallet_object_store::Call<Runtime>,
) {
    if let Some(call_object) = call.extract_call_object() {
        objects.push(BlockObject::V0 {
            hash: call_object.hash,
            offset: base_offset + call_object.offset,
        });
    }
}

fn extract_utility_block_object_mapping<I: Iterator<Item = Hash>>(
    mut base_offset: u32,
    objects: &mut Vec<BlockObject>,
    call: &pallet_utility::Call<Runtime>,
    mut recursion_depth_left: u16,
    successful_calls: &mut Peekable<I>,
) {
    if recursion_depth_left == 0 {
        return;
    }

    recursion_depth_left -= 1;

    // Add enum variant to the base offset.
    base_offset += 1;

    match call {
        pallet_utility::Call::batch { calls }
        | pallet_utility::Call::batch_all { calls }
        | pallet_utility::Call::force_batch { calls } => {
            base_offset += Compact::compact_len(&(calls.len() as u32)) as u32;

            for call in calls {
                extract_call_block_object_mapping(
                    base_offset,
                    objects,
                    call,
                    recursion_depth_left,
                    successful_calls,
                );

                base_offset += call.encoded_size() as u32;
            }
        }
        pallet_utility::Call::as_derivative { index, call } => {
            base_offset += index.encoded_size() as u32;

            extract_call_block_object_mapping(
                base_offset,
                objects,
                call.as_ref(),
                recursion_depth_left,
                successful_calls,
            );
        }
        pallet_utility::Call::dispatch_as { as_origin, call } => {
            base_offset += as_origin.encoded_size() as u32;

            extract_call_block_object_mapping(
                base_offset,
                objects,
                call.as_ref(),
                recursion_depth_left,
                successful_calls,
            );
        }
        pallet_utility::Call::__Ignore(_, _) => {
            // Ignore.
        }
    }
}

fn extract_call_block_object_mapping<I: Iterator<Item = Hash>>(
    mut base_offset: u32,
    objects: &mut Vec<BlockObject>,
    call: &Call,
    recursion_depth_left: u16,
    successful_calls: &mut Peekable<I>,
) {
    // Add enum variant to the base offset.
    base_offset += 1;

    match call {
        Call::Feeds(call) => {
            extract_feeds_block_object_mapping(base_offset, objects, call, successful_calls);
        }
        Call::ObjectStore(call) => {
            extract_object_store_block_object_mapping(base_offset, objects, call);
        }
        Call::Utility(call) => {
            extract_utility_block_object_mapping(
                base_offset,
                objects,
                call,
                recursion_depth_left,
                successful_calls,
            );
        }
        _ => {}
    }
}

fn extract_block_object_mapping(block: Block, successful_calls: Vec<Hash>) -> BlockObjectMapping {
    let mut block_object_mapping = BlockObjectMapping::default();
    let mut successful_calls = successful_calls.into_iter().peekable();
    let mut base_offset =
        block.header.encoded_size() + Compact::compact_len(&(block.extrinsics.len() as u32));
    for extrinsic in block.extrinsics {
        let signature_size = extrinsic
            .signature
            .as_ref()
            .map(|s| s.encoded_size())
            .unwrap_or_default();
        // Extrinsic starts with vector length and version byte, followed by optional signature and
        // `function` encoding.
        let base_extrinsic_offset = base_offset
            + Compact::compact_len(
                &((1 + signature_size + extrinsic.function.encoded_size()) as u32),
            )
            + 1
            + signature_size;

        extract_call_block_object_mapping(
            base_extrinsic_offset as u32,
            &mut block_object_mapping.objects,
            &extrinsic.function,
            MAX_OBJECT_MAPPING_RECURSION_DEPTH,
            &mut successful_calls,
        );

        base_offset += extrinsic.encoded_size();
    }

    block_object_mapping
}

fn extract_bundles(extrinsics: Vec<OpaqueExtrinsic>) -> Vec<OpaqueBundle> {
    extrinsics
        .into_iter()
        .filter_map(|opaque_extrinsic| {
            match <UncheckedExtrinsic>::decode(&mut opaque_extrinsic.encode().as_slice()) {
                Ok(uxt) => {
                    if let Call::Executor(pallet_executor::Call::submit_transaction_bundle {
                        signed_opaque_bundle,
                    }) = uxt.function
                    {
                        Some(signed_opaque_bundle.opaque_bundle)
                    } else {
                        None
                    }
                }
                Err(_) => None,
            }
        })
        .collect()
}

fn extrinsics_shuffling_seed<Block: BlockT>(header: Block::Header) -> Randomness {
    if header.number().is_zero() {
        Randomness::default()
    } else {
        let mut pre_digest: Option<_> = None;
        for log in header.digest().logs() {
            match (
                log.as_subspace_pre_digest::<FarmerPublicKey>(),
                pre_digest.is_some(),
            ) {
                (Some(_), true) => panic!("Multiple Subspace pre-runtime digests in a header"),
                (None, _) => {}
                (s, false) => pre_digest = s,
            }
        }

        let pre_digest = pre_digest.expect("Header must contain one pre-runtime digest; qed");

        let seed: &[u8] = b"extrinsics-shuffling-seed";
        let randomness = derive_randomness(
            &pre_digest.solution.public_key,
            pre_digest.solution.tag,
            &pre_digest.solution.tag_signature,
        )
        .expect("Tag signature is verified by the client and must always be valid; qed");
        let mut data = Vec::with_capacity(seed.len() + randomness.len());
        data.extend_from_slice(seed);
        data.extend_from_slice(&randomness);

        BlakeTwo256::hash_of(&data).into()
    }
}

struct RewardAddress([u8; 32]);

impl From<FarmerPublicKey> for RewardAddress {
    fn from(farmer_public_key: FarmerPublicKey) -> Self {
        Self(
            farmer_public_key
                .as_slice()
                .try_into()
                .expect("Public key is always of correct size; qed"),
        )
    }
}

impl From<RewardAddress> for AccountId32 {
    fn from(reward_address: RewardAddress) -> Self {
        reward_address.0.into()
    }
}

impl_runtime_apis! {
    impl sp_api::Core<Block> for Runtime {
        fn version() -> RuntimeVersion {
            VERSION
        }

        fn execute_block(block: Block) {
            Executive::execute_block(block);
        }

        fn initialize_block(header: &<Block as BlockT>::Header) {
            Executive::initialize_block(header)
        }
    }

    impl sp_api::Metadata<Block> for Runtime {
        fn metadata() -> OpaqueMetadata {
            OpaqueMetadata::new(Runtime::metadata().into())
        }
    }

    impl sp_block_builder::BlockBuilder<Block> for Runtime {
        fn apply_extrinsic(extrinsic: <Block as BlockT>::Extrinsic) -> ApplyExtrinsicResult {
            Executive::apply_extrinsic(extrinsic)
        }

        fn finalize_block() -> <Block as BlockT>::Header {
            Executive::finalize_block()
        }

        fn inherent_extrinsics(data: sp_inherents::InherentData) -> Vec<<Block as BlockT>::Extrinsic> {
            data.create_extrinsics()
        }

        fn check_inherents(
            block: Block,
            data: sp_inherents::InherentData,
        ) -> sp_inherents::CheckInherentsResult {
            data.check_extrinsics(&block)
        }
    }

    impl sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block> for Runtime {
        fn validate_transaction(
            source: TransactionSource,
            tx: <Block as BlockT>::Extrinsic,
            block_hash: <Block as BlockT>::Hash,
        ) -> TransactionValidity {
            Executive::validate_transaction(source, tx, block_hash)
        }
    }

    impl sp_offchain::OffchainWorkerApi<Block> for Runtime {
        fn offchain_worker(header: &<Block as BlockT>::Header) {
            Executive::offchain_worker(header)
        }
    }

    impl sp_objects::ObjectsApi<Block> for Runtime {
        fn extract_block_object_mapping(block: Block, successful_calls: Vec<Hash>) -> BlockObjectMapping {
            extract_block_object_mapping(block, successful_calls)
        }

        fn validated_object_call_hashes() -> Vec<Hash> {
            Feeds::successful_puts()
        }
    }

    impl sp_consensus_subspace::SubspaceApi<Block, FarmerPublicKey> for Runtime {
        fn confirmation_depth_k() -> <<Block as BlockT>::Header as HeaderT>::Number {
            <Self as pallet_subspace::Config>::ConfirmationDepthK::get()
        }

        fn total_pieces() -> u64 {
            <pallet_subspace::Pallet<Runtime>>::total_pieces()
        }

        fn record_size() -> u32 {
            <Self as pallet_subspace::Config>::RecordSize::get()
        }

        fn max_plot_size() -> u64 {
            <Self as pallet_subspace::Config>::MaxPlotSize::get()
        }

        fn recorded_history_segment_size() -> u32 {
            <Self as pallet_subspace::Config>::RecordedHistorySegmentSize::get()
        }

        fn slot_duration() -> Duration {
            Duration::from_millis(Subspace::slot_duration())
        }

        fn global_randomnesses() -> GlobalRandomnesses {
            Subspace::global_randomnesses()
        }

        fn solution_ranges() -> SolutionRanges {
            Subspace::solution_ranges()
        }

        fn salts() -> Salts {
            Subspace::salts()
        }

        fn submit_report_equivocation_extrinsic(
            equivocation_proof: EquivocationProof<<Block as BlockT>::Header>,
        ) -> Option<()> {
            Subspace::submit_equivocation_report(equivocation_proof)
        }

        fn submit_vote_extrinsic(
            signed_vote: SignedVote<NumberFor<Block>, <Block as BlockT>::Hash, FarmerPublicKey>,
        ) {
            let SignedVote { vote, signature } = signed_vote;
            let Vote::V0 {
                height,
                parent_hash,
                slot,
                solution,
            } = vote;

            Subspace::submit_vote(SignedVote {
                vote: Vote::V0 {
                    height,
                    parent_hash,
                    slot,
                    solution: solution.into_reward_address_format::<RewardAddress, AccountId32>(),
                },
                signature,
            })
        }

        fn is_in_block_list(farmer_public_key: &FarmerPublicKey) -> bool {
            // TODO: Either check tx pool too for pending equivocations or replace equivocation
            //  mechanism with an alternative one, so that blocking happens faster
            Subspace::is_in_block_list(farmer_public_key)
        }

        fn records_root(segment_index: u64) -> Option<Sha256Hash> {
            Subspace::records_root(segment_index)
        }

        fn extract_root_blocks(ext: &<Block as BlockT>::Extrinsic) -> Option<Vec<RootBlock>> {
            extract_root_blocks(ext)
        }
    }

    impl sp_executor::ExecutorApi<Block, cirrus_primitives::Hash> for Runtime {
        fn submit_execution_receipt_unsigned(
            execution_receipt: sp_executor::SignedExecutionReceipt<NumberFor<Block>, <Block as BlockT>::Hash, cirrus_primitives::Hash>,
        ) {
            Executor::submit_execution_receipt_unsigned(execution_receipt)
        }

        fn submit_transaction_bundle_unsigned(opaque_bundle: sp_executor::SignedOpaqueBundle) {
            Executor::submit_transaction_bundle_unsigned(opaque_bundle)
        }

        fn submit_fraud_proof_unsigned(fraud_proof: FraudProof) {
            Executor::submit_fraud_proof_unsigned(fraud_proof)
        }

        fn submit_bundle_equivocation_proof_unsigned(
            bundle_equivocation_proof: sp_executor::BundleEquivocationProof,
        ) {
            Executor::submit_bundle_equivocation_proof_unsigned(bundle_equivocation_proof)
        }

        fn submit_invalid_transaction_proof_unsigned(
            invalid_transaction_proof: sp_executor::InvalidTransactionProof,
        ) {
            Executor::submit_invalid_transaction_proof_unsigned(invalid_transaction_proof)
        }

        fn extract_bundles(extrinsics: Vec<OpaqueExtrinsic>) -> Vec<OpaqueBundle> {
            extract_bundles(extrinsics)
        }

        fn extrinsics_shuffling_seed(header: <Block as BlockT>::Header) -> Randomness {
            extrinsics_shuffling_seed::<Block>(header)
        }

        fn execution_wasm_bundle() -> Cow<'static, [u8]> {
            EXECUTION_WASM_BUNDLE.into()
        }

        fn executor_id() -> sp_executor::ExecutorId {
            Executor::executor()
                .map(|(_account_id, executor_id)| executor_id)
                .expect("Executor must be provided; qed")
        }

        fn best_execution_chain_number() -> NumberFor<Block> {
            Executor::best_execution_chain_number()
        }

        fn maximum_receipt_drift() -> NumberFor<Block> {
            MaximumReceiptDrift::get()
        }
    }

    impl sp_session::SessionKeys<Block> for Runtime {
        fn generate_session_keys(seed: Option<Vec<u8>>) -> Vec<u8> {
            SessionKeys::generate(seed)
        }

        fn decode_session_keys(
            encoded: Vec<u8>,
        ) -> Option<Vec<(Vec<u8>, KeyTypeId)>> {
            SessionKeys::decode_into_raw_public_keys(&encoded)
        }
    }

    impl frame_system_rpc_runtime_api::AccountNonceApi<Block, AccountId, Index> for Runtime {
        fn account_nonce(account: AccountId) -> Index {
            System::account_nonce(account)
        }
    }

    impl pallet_transaction_payment_rpc_runtime_api::TransactionPaymentApi<Block, Balance> for Runtime {
        fn query_info(
            uxt: <Block as BlockT>::Extrinsic,
            len: u32,
        ) -> pallet_transaction_payment_rpc_runtime_api::RuntimeDispatchInfo<Balance> {
            TransactionPayment::query_info(uxt, len)
        }
        fn query_fee_details(
            uxt: <Block as BlockT>::Extrinsic,
            len: u32,
        ) -> pallet_transaction_payment::FeeDetails<Balance> {
            TransactionPayment::query_fee_details(uxt, len)
        }
    }
}
