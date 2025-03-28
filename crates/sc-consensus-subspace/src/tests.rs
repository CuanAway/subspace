// Copyright (C) 2019-2021 Parity Technologies (UK) Ltd.
// Copyright (C) 2021 Subspace Labs, Inc.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

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

//! PoC testsuite

use crate::{
    find_pre_digest, start_subspace, Config, NewSlotNotification, SubspaceLink, SubspaceParams,
    SubspaceVerifier,
};
use codec::Encode;
use futures::channel::oneshot;
use futures::executor::block_on;
use futures::{future, SinkExt, StreamExt};
use log::{debug, trace};
use parking_lot::Mutex;
use rand::prelude::*;
use sc_block_builder::{BlockBuilder, BlockBuilderProvider};
use sc_client_api::backend::TransactionFor;
use sc_client_api::{BlockBackend, BlockchainEvents};
use sc_consensus::block_import::ForkChoiceStrategy;
use sc_consensus::{
    BlockCheckParams, BlockImport, BlockImportParams, BoxBlockImport, BoxJustificationImport,
    ImportResult, Verifier,
};
use sc_consensus_slots::{BackoffAuthoringOnFinalizedHeadLagging, SlotProportion};
use sc_network::config::ProtocolConfig;
use sc_network_test::{
    BlockImportAdapter, Peer, PeersClient, PeersFullClient, TestClientBuilder,
    TestClientBuilderExt, TestNetFactory,
};
use sc_service::TaskManager;
use schnorrkel::Keypair;
use sp_api::{HeaderT, ProvideRuntimeApi};
use sp_consensus::{
    AlwaysCanAuthor, BlockOrigin, CacheKeyId, DisableProofRecording, Environment,
    NoNetwork as DummyOracle, Proposal, Proposer,
};
use sp_consensus_slots::{Slot, SlotDuration};
use sp_consensus_subspace::digests::{
    CompatibleDigestItem, PreDigest, SaltDescriptor, SolutionRangeDescriptor,
};
use sp_consensus_subspace::inherents::InherentDataProvider;
use sp_consensus_subspace::{FarmerPublicKey, FarmerSignature, SubspaceApi};
use sp_core::crypto::UncheckedFrom;
use sp_inherents::{CreateInherentDataProviders, InherentData};
use sp_runtime::generic::{BlockId, Digest, DigestItem};
use sp_runtime::traits::{Block as BlockT, Zero};
use sp_timestamp::InherentDataProvider as TimestampInherentDataProvider;
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use subspace_archiving::archiver::Archiver;
use subspace_core_primitives::objects::BlockObjectMapping;
use subspace_core_primitives::{FlatPieces, LocalChallenge, Piece, Solution, Tag, TagSignature};
use subspace_solving::{
    create_tag, create_tag_signature, derive_local_challenge, SubspaceCodec, REWARD_SIGNING_CONTEXT,
};
use substrate_test_runtime::{Block as TestBlock, Hash};

type TestClient = substrate_test_runtime_client::client::Client<
    substrate_test_runtime_client::Backend,
    substrate_test_runtime_client::ExecutorDispatch,
    TestBlock,
    substrate_test_runtime_client::runtime::RuntimeApi,
>;

#[derive(Copy, Clone, PartialEq)]
enum Stage {
    PreSeal,
    PostSeal,
}

type Mutator = Arc<dyn Fn(&mut TestHeader, Stage) + Send + Sync>;

#[derive(Clone)]
pub struct TestCreateInherentDataProviders {
    inner: Arc<
        dyn CreateInherentDataProviders<
            TestBlock,
            SubspaceLink<TestBlock>,
            InherentDataProviders = (TimestampInherentDataProvider, InherentDataProvider),
        >,
    >,
}

#[async_trait::async_trait]
impl CreateInherentDataProviders<TestBlock, SubspaceLink<TestBlock>>
    for TestCreateInherentDataProviders
{
    type InherentDataProviders = (TimestampInherentDataProvider, InherentDataProvider);

    async fn create_inherent_data_providers(
        &self,
        parent: <TestBlock as BlockT>::Hash,
        extra_args: SubspaceLink<TestBlock>,
    ) -> Result<Self::InherentDataProviders, Box<dyn std::error::Error + Send + Sync>> {
        self.inner
            .create_inherent_data_providers(parent, extra_args)
            .await
    }
}

type SubspaceBlockImport = PanickingBlockImport<
    crate::SubspaceBlockImport<
        TestBlock,
        TestClient,
        Arc<TestClient>,
        AlwaysCanAuthor,
        TestCreateInherentDataProviders,
    >,
>;

type SubspaceProposal =
    Proposal<TestBlock, TransactionFor<substrate_test_runtime_client::Backend, TestBlock>, ()>;

#[derive(Clone)]
struct DummyFactory {
    client: Arc<TestClient>,
    mutator: Mutator,
}

struct DummyProposer {
    factory: DummyFactory,
    parent_hash: Hash,
}

impl Environment<TestBlock> for DummyFactory {
    type CreateProposer = future::Ready<Result<DummyProposer, Self::Error>>;
    type Proposer = DummyProposer;
    type Error = sp_blockchain::Error;

    fn init(&mut self, parent_header: &<TestBlock as BlockT>::Header) -> Self::CreateProposer {
        future::ready(Ok(DummyProposer {
            factory: self.clone(),
            parent_hash: parent_header.hash(),
        }))
    }
}

impl DummyProposer {
    fn propose_with(
        &mut self,
        pre_digests: Digest,
    ) -> future::Ready<Result<SubspaceProposal, sp_blockchain::Error>> {
        let block_builder = self
            .factory
            .client
            .new_block_at(&BlockId::Hash(self.parent_hash), pre_digests, false)
            .unwrap();

        let mut block = match block_builder.build() {
            Ok(b) => b.block,
            Err(e) => return future::ready(Err(e)),
        };

        {
            let digest = DigestItem::solution_range_descriptor(SolutionRangeDescriptor {
                solution_range: u64::MAX,
            });
            block.header.digest_mut().push(digest);
        }
        {
            let digest = DigestItem::salt_descriptor(SaltDescriptor {
                salt: 0u64.to_le_bytes(),
            });
            block.header.digest_mut().push(digest);
        }

        // mutate the block header according to the mutator.
        (self.factory.mutator)(&mut block.header, Stage::PreSeal);

        future::ready(Ok(Proposal {
            block,
            proof: (),
            storage_changes: Default::default(),
        }))
    }
}

impl Proposer<TestBlock> for DummyProposer {
    type Error = sp_blockchain::Error;
    type Transaction = TransactionFor<substrate_test_runtime_client::Backend, TestBlock>;
    type Proposal = future::Ready<Result<SubspaceProposal, Self::Error>>;
    type ProofRecording = DisableProofRecording;
    type Proof = ();

    fn propose(
        mut self,
        _: InherentData,
        pre_digests: Digest,
        _: Duration,
        _: Option<usize>,
    ) -> Self::Proposal {
        self.propose_with(pre_digests)
    }
}

thread_local! {
    static MUTATOR: RefCell<Mutator> = RefCell::new(Arc::new(|_, _|()));
}

#[derive(Clone)]
pub struct PanickingBlockImport<B> {
    block_import: B,
    link: SubspaceLink<TestBlock>,
}

#[async_trait::async_trait]
impl<B: BlockImport<TestBlock>> BlockImport<TestBlock> for PanickingBlockImport<B>
where
    B::Transaction: Send,
    B: Send,
{
    type Error = B::Error;
    type Transaction = B::Transaction;

    async fn import_block(
        &mut self,
        block: BlockImportParams<TestBlock, Self::Transaction>,
        new_cache: HashMap<CacheKeyId, Vec<u8>>,
    ) -> Result<ImportResult, Self::Error> {
        // TODO: Here we are hacking around lack of transaction support in test runtime and
        //  remove known root blocks for current block to make sure block import doesn't fail, this
        //  should be removed once runtime supports transactions
        let block_number = block.header.number;
        let removed_root_blocks = self.link.root_blocks.lock().pop(&block_number);

        let import_result = self
            .block_import
            .import_block(block, new_cache)
            .await
            .expect("importing block failed");

        if let Some(removed_root_blocks) = removed_root_blocks {
            self.link
                .root_blocks
                .lock()
                .put(block_number, removed_root_blocks);
        }

        Ok(import_result)
    }

    async fn check_block(
        &mut self,
        block: BlockCheckParams<TestBlock>,
    ) -> Result<ImportResult, Self::Error> {
        Ok(self
            .block_import
            .check_block(block)
            .await
            .expect("checking block failed"))
    }
}

type SubspacePeer = Peer<Option<PeerData>, SubspaceBlockImport>;

pub struct SubspaceTestNet {
    peers: Vec<SubspacePeer>,
}

type TestHeader = <TestBlock as BlockT>::Header;

type TestSelectChain =
    substrate_test_runtime_client::LongestChain<substrate_test_runtime_client::Backend, TestBlock>;

pub struct TestVerifier {
    inner: SubspaceVerifier<
        TestBlock,
        PeersFullClient,
        TestSelectChain,
        Box<dyn Fn() -> Slot + Send + Sync + 'static>,
    >,
    mutator: Mutator,
}

#[async_trait::async_trait]
impl Verifier<TestBlock> for TestVerifier {
    /// Verify the given data and return the BlockImportParams and an optional
    /// new set of validators to import. If not, err with an Error-Message
    /// presented to the User in the logs.
    async fn verify(
        &mut self,
        mut block: BlockImportParams<TestBlock, ()>,
    ) -> Result<
        (
            BlockImportParams<TestBlock, ()>,
            Option<Vec<(CacheKeyId, Vec<u8>)>>,
        ),
        String,
    > {
        // apply post-sealing mutations (i.e. stripping seal, if desired).
        (self.mutator)(&mut block.header, Stage::PostSeal);
        self.inner.verify(block).await
    }
}

pub struct PeerData {
    link: SubspaceLink<TestBlock>,
    block_import: Mutex<
        Option<
            BoxBlockImport<
                TestBlock,
                TransactionFor<substrate_test_runtime_client::Backend, TestBlock>,
            >,
        >,
    >,
}

impl TestNetFactory for SubspaceTestNet {
    type Verifier = TestVerifier;
    type PeerData = Option<PeerData>;
    type BlockImport = SubspaceBlockImport;

    /// Create new test network with peers and given config.
    fn from_config(_config: &ProtocolConfig) -> Self {
        debug!(target: "subspace", "Creating test network from config");
        SubspaceTestNet { peers: Vec::new() }
    }

    fn make_block_import(
        &self,
        client: PeersClient,
    ) -> (
        BlockImportAdapter<Self::BlockImport>,
        Option<BoxJustificationImport<TestBlock>>,
        Option<PeerData>,
    ) {
        let client = client.as_client();

        let config = Config::get(&*client).expect("config available");
        let (block_import, link) = crate::block_import(
            config,
            client.clone(),
            client,
            AlwaysCanAuthor,
            TestCreateInherentDataProviders {
                inner: Arc::new(|_, _| async {
                    let timestamp = TimestampInherentDataProvider::from_system_time();
                    let slot = InherentDataProvider::from_timestamp_and_slot_duration(
                        *timestamp,
                        SlotDuration::from_millis(6000),
                        vec![],
                    );

                    Ok((timestamp, slot))
                }),
            },
        )
        .expect("can initialize block-import");

        let block_import = PanickingBlockImport {
            block_import,
            link: link.clone(),
        };

        let data_block_import =
            Mutex::new(Some(Box::new(block_import.clone()) as BoxBlockImport<_, _>));
        (
            BlockImportAdapter::new(block_import),
            None,
            Some(PeerData {
                link,
                block_import: data_block_import,
            }),
        )
    }

    fn make_verifier(
        &self,
        client: PeersClient,
        _cfg: &ProtocolConfig,
        _maybe_link: &Option<PeerData>,
    ) -> Self::Verifier {
        use substrate_test_runtime_client::DefaultTestClientBuilderExt;

        let client = client.as_client();
        trace!(target: "subspace", "Creating a verifier");

        let (_, longest_chain) = TestClientBuilder::new().build_with_longest_chain();

        TestVerifier {
            inner: SubspaceVerifier {
                client,
                select_chain: longest_chain,
                slot_now: Box::new(|| {
                    let timestamp = TimestampInherentDataProvider::from_system_time();

                    Slot::from_timestamp(*timestamp, SlotDuration::from_millis(6000))
                }),
                telemetry: None,
                reward_signing_context: schnorrkel::context::signing_context(
                    REWARD_SIGNING_CONTEXT,
                ),
                block: PhantomData::default(),
            },
            mutator: MUTATOR.with(|m| m.borrow().clone()),
        }
    }

    fn peer(&mut self, i: usize) -> &mut SubspacePeer {
        trace!(target: "subspace", "Retrieving a peer");
        &mut self.peers[i]
    }

    fn peers(&self) -> &Vec<SubspacePeer> {
        trace!(target: "subspace", "Retrieving peers");
        &self.peers
    }

    fn mut_peers<F: FnOnce(&mut Vec<SubspacePeer>)>(&mut self, closure: F) {
        closure(&mut self.peers);
    }
}

#[test]
#[should_panic]
fn rejects_empty_block() {
    sp_tracing::try_init_simple();
    let mut net = SubspaceTestNet::new(3);
    let block_builder = |builder: BlockBuilder<_, _, _>| builder.build().unwrap().block;
    net.mut_peers(|peer| {
        peer[0].generate_blocks(1, BlockOrigin::NetworkInitialSync, block_builder);
    })
}

fn get_archived_pieces(client: &TestClient) -> Vec<FlatPieces> {
    let genesis_block_id = BlockId::Number(Zero::zero());
    let runtime_api = client.runtime_api();

    let record_size = runtime_api.record_size(&genesis_block_id).unwrap();
    let recorded_history_segment_size = runtime_api
        .recorded_history_segment_size(&genesis_block_id)
        .unwrap();

    let mut archiver = Archiver::new(record_size as usize, recorded_history_segment_size as usize)
        .expect("Incorrect parameters for archiver");

    let genesis_block = client.block(&genesis_block_id).unwrap().unwrap();
    archiver
        .add_block(genesis_block.encode(), BlockObjectMapping::default())
        .into_iter()
        .map(|archived_segment| archived_segment.pieces)
        .collect()
}

fn run_one_test(mutator: impl Fn(&mut TestHeader, Stage) + Send + Sync + 'static) {
    sp_tracing::try_init_simple();
    let mutator = Arc::new(mutator) as Mutator;

    MUTATOR.with(|m| *m.borrow_mut() = mutator.clone());
    let net = SubspaceTestNet::new(3);

    let net = Arc::new(Mutex::new(net));
    let mut import_notifications = Vec::new();
    let mut subspace_futures = Vec::<Pin<Box<dyn Future<Output = ()>>>>::new();
    let tokio_runtime = sc_cli::build_runtime().unwrap();

    for peer_id in [0, 1, 2_usize].iter() {
        let mut net = net.lock();
        let peer = net.peer(*peer_id);
        let client = peer.client().as_client().clone();
        let select_chain = peer.select_chain().expect("Full client has select_chain");

        let mut got_own = false;
        let mut got_other = false;

        let data = peer
            .data
            .as_ref()
            .expect("Subspace link set up during initialization");

        let environ = DummyFactory {
            client: client.clone(),
            mutator: mutator.clone(),
        };

        import_notifications.push(
            // run each future until we get one of our own blocks with number higher than 5
            // that was produced locally.
            client
                .import_notification_stream()
                .take_while(move |n| {
                    future::ready(
                        n.header.number() < &5 || {
                            if n.origin == BlockOrigin::Own {
                                got_own = true;
                            } else {
                                got_other = true;
                            }

                            // continue until we have at least one block of our own
                            // and one of another peer.
                            !(got_own && got_other)
                        },
                    )
                })
                .for_each(|_| future::ready(())),
        );

        let task_manager = TaskManager::new(tokio_runtime.handle().clone(), None).unwrap();

        super::start_subspace_archiver(
            &data.link,
            client.clone(),
            &task_manager.spawn_essential_handle(),
            false,
        );

        let (archived_pieces_sender, archived_pieces_receiver) = oneshot::channel();

        std::thread::spawn({
            let client = Arc::clone(&client);

            move || {
                let archived_pieces = get_archived_pieces(&client);
                archived_pieces_sender.send(archived_pieces).unwrap();
            }
        });

        let subspace_worker = start_subspace(SubspaceParams {
            block_import: data
                .block_import
                .lock()
                .take()
                .expect("import set up during init"),
            select_chain,
            client,
            env: environ,
            sync_oracle: DummyOracle,
            create_inherent_data_providers: Box::new(|_, _| async {
                let timestamp = TimestampInherentDataProvider::from_system_time();
                let slot = InherentDataProvider::from_timestamp_and_slot_duration(
                    *timestamp,
                    SlotDuration::from_millis(6000),
                    vec![],
                );

                Ok((timestamp, slot))
            }),
            force_authoring: false,
            backoff_authoring_blocks: Some(BackoffAuthoringOnFinalizedHeadLagging::default()),
            subspace_link: data.link.clone(),
            can_author_with: sp_consensus::AlwaysCanAuthor,
            justification_sync_link: (),
            block_proposal_slot_portion: SlotProportion::new(0.5),
            max_block_proposal_slot_portion: None,
            telemetry: None,
        })
        .expect("Starts Subspace");

        let mut new_slot_notification_stream = data.link.new_slot_notification_stream().subscribe();
        let subspace_farmer = async move {
            let keypair = Keypair::generate();
            let subspace_codec = SubspaceCodec::new(keypair.public.as_ref());
            let (piece_index, mut encoding) = archived_pieces_receiver
                .await
                .unwrap()
                .iter()
                .flat_map(|flat_pieces| flat_pieces.as_pieces())
                .enumerate()
                .choose(&mut rand::thread_rng())
                .map(|(piece_index, piece)| (piece_index as u64, Piece::try_from(piece).unwrap()))
                .unwrap();
            subspace_codec.encode(&mut encoding, piece_index).unwrap();

            while let Some(NewSlotNotification {
                new_slot_info,
                mut solution_sender,
            }) = new_slot_notification_stream.next().await
            {
                if Into::<u64>::into(new_slot_info.slot) % 3 == (*peer_id) as u64 {
                    let tag: Tag = create_tag(&encoding, new_slot_info.salt);

                    let _ = solution_sender
                        .send(Solution {
                            public_key: FarmerPublicKey::unchecked_from(keypair.public.to_bytes()),
                            reward_address: FarmerPublicKey::unchecked_from(
                                keypair.public.to_bytes(),
                            ),
                            piece_index,
                            encoding: encoding.clone(),
                            tag_signature: create_tag_signature(&keypair, tag),
                            local_challenge: derive_local_challenge(
                                &keypair,
                                new_slot_info.global_challenge,
                            ),
                            tag,
                        })
                        .await;
                }
            }
        };

        subspace_futures.push(Box::pin(subspace_worker));
        subspace_futures.push(Box::pin(subspace_farmer));
    }
    tokio_runtime.block_on(future::select(
        futures::future::poll_fn(move |cx| {
            let mut net = net.lock();
            net.poll(cx);
            for p in net.peers() {
                for (h, e) in p.failed_verifications() {
                    panic!("Verification failed for {:?}: {}", h, e);
                }
            }

            Poll::<()>::Pending
        }),
        future::select(
            future::join_all(import_notifications),
            future::join_all(subspace_futures),
        ),
    ));
}

// TODO: Un-ignore once `submit_test_store_root_block()` is working or transactions are supported in
//  test runtime
#[test]
#[ignore]
fn authoring_blocks() {
    run_one_test(|_, _| ())
}

#[test]
#[should_panic]
fn rejects_missing_inherent_digest() {
    run_one_test(|header: &mut TestHeader, stage| {
        let v = std::mem::take(&mut header.digest_mut().logs);
        header.digest_mut().logs = v
            .into_iter()
            .filter(|v| {
                stage == Stage::PostSeal
                    || DigestItem::as_subspace_pre_digest::<FarmerPublicKey>(v).is_none()
            })
            .collect()
    })
}

#[test]
#[should_panic]
fn rejects_missing_seals() {
    run_one_test(|header: &mut TestHeader, stage| {
        let v = std::mem::take(&mut header.digest_mut().logs);
        header.digest_mut().logs = v
            .into_iter()
            .filter(|v| stage == Stage::PreSeal || DigestItem::as_subspace_seal(v).is_none())
            .collect()
    })
}

#[test]
fn wrong_consensus_engine_id_rejected() {
    sp_tracing::try_init_simple();
    let keypair = Keypair::generate();
    let ctx = schnorrkel::context::signing_context(REWARD_SIGNING_CONTEXT);
    let bad_seal = DigestItem::Seal([0; 4], keypair.sign(ctx.bytes(b"")).to_bytes().to_vec());
    assert!(CompatibleDigestItem::as_subspace_pre_digest::<FarmerPublicKey>(&bad_seal).is_none());
    assert!(CompatibleDigestItem::as_subspace_seal(&bad_seal).is_none())
}

#[test]
fn malformed_pre_digest_rejected() {
    sp_tracing::try_init_simple();
    let bad_seal = DigestItem::subspace_seal(FarmerSignature::unchecked_from([0u8; 64]));
    assert!(CompatibleDigestItem::as_subspace_pre_digest::<FarmerPublicKey>(&bad_seal).is_none());
}

#[test]
fn sig_is_not_pre_digest() {
    sp_tracing::try_init_simple();
    let keypair = Keypair::generate();
    let ctx = schnorrkel::context::signing_context(REWARD_SIGNING_CONTEXT);
    let bad_seal = DigestItem::subspace_seal(FarmerSignature::unchecked_from(
        keypair.sign(ctx.bytes(b"")).to_bytes(),
    ));
    assert!(CompatibleDigestItem::as_subspace_pre_digest::<FarmerPublicKey>(&bad_seal).is_none());
    assert!(CompatibleDigestItem::as_subspace_seal(&bad_seal).is_some())
}

/// Claims the given slot number. always returning a dummy block.
pub fn dummy_claim_slot(
    slot: Slot,
) -> Option<(PreDigest<FarmerPublicKey, FarmerPublicKey>, FarmerPublicKey)> {
    Some((
        PreDigest {
            solution: Solution {
                public_key: FarmerPublicKey::unchecked_from([0u8; 32]),
                reward_address: FarmerPublicKey::unchecked_from([0u8; 32]),
                piece_index: 0,
                encoding: Piece::default(),
                tag_signature: TagSignature {
                    output: [0; 32],
                    proof: [0; 64],
                },
                local_challenge: LocalChallenge {
                    output: [0; 32],
                    proof: [0; 64],
                },
                tag: Tag::default(),
            },
            slot,
        },
        FarmerPublicKey::unchecked_from([0u8; 32]),
    ))
}

#[test]
fn can_author_block() {
    sp_tracing::try_init_simple();

    let mut i = 0;

    // we might need to try a couple of times
    loop {
        match dummy_claim_slot(i.into()) {
            None => i += 1,
            Some(s) => {
                debug!(target: "subspace", "Authored block {:?}", s.0);
                break;
            }
        }
    }
}

// Propose and import a new Subspace block on top of the given parent.
fn propose_and_import_block<Transaction: Send + 'static>(
    parent: &TestHeader,
    slot: Option<Slot>,
    proposer_factory: &mut DummyFactory,
    block_import: &mut BoxBlockImport<TestBlock, Transaction>,
) -> sp_core::H256 {
    let mut proposer = futures::executor::block_on(proposer_factory.init(parent)).unwrap();

    let slot = slot.unwrap_or_else(|| {
        let parent_pre_digest = find_pre_digest::<TestHeader>(parent).unwrap();
        parent_pre_digest.slot + 1
    });

    let keypair = Keypair::generate();
    let ctx = schnorrkel::context::signing_context(REWARD_SIGNING_CONTEXT);

    let (pre_digest, signature) = {
        let encoding = Piece::default();
        let tag: Tag = [0u8; 8];

        (
            sp_runtime::generic::Digest {
                logs: vec![DigestItem::subspace_pre_digest(&PreDigest {
                    slot,
                    solution: Solution {
                        public_key: FarmerPublicKey::unchecked_from(keypair.public.to_bytes()),
                        reward_address: FarmerPublicKey::unchecked_from(keypair.public.to_bytes()),
                        piece_index: 0,
                        encoding,
                        tag_signature: create_tag_signature(&keypair, tag),
                        local_challenge: LocalChallenge {
                            output: [0; 32],
                            proof: [0; 64],
                        },
                        tag,
                    },
                })],
            },
            keypair.sign(ctx.bytes(&[])).to_bytes(),
        )
    };

    let mut block = futures::executor::block_on(proposer.propose_with(pre_digest))
        .unwrap()
        .block;

    let seal = DigestItem::subspace_seal(signature.to_vec().try_into().unwrap());

    let post_hash = {
        block.header.digest_mut().push(seal.clone());
        let h = block.header.hash();
        block.header.digest_mut().pop();
        h
    };

    let mut import = BlockImportParams::new(BlockOrigin::Own, block.header);
    import.post_digests.push(seal);
    import.body = Some(block.extrinsics);
    import.fork_choice = Some(ForkChoiceStrategy::LongestChain);
    let import_result = block_on(block_import.import_block(import, Default::default())).unwrap();

    match import_result {
        ImportResult::Imported(_) => {}
        _ => panic!("expected block to be imported"),
    }

    post_hash
}

#[test]
#[should_panic]
fn verify_slots_are_strictly_increasing() {
    let mut net = SubspaceTestNet::new(1);

    let peer = net.peer(0);
    let data = peer
        .data
        .as_ref()
        .expect("Subspace link set up during initialization");

    let client = peer.client().as_client();
    let mut block_import = data
        .block_import
        .lock()
        .take()
        .expect("import set up during init");

    let mut proposer_factory = DummyFactory {
        client: client.clone(),
        mutator: Arc::new(|_, _| ()),
    };

    let genesis_header = client.header(&BlockId::Number(0)).unwrap().unwrap();

    // we should have no issue importing this block
    let b1 = propose_and_import_block(
        &genesis_header,
        Some(999.into()),
        &mut proposer_factory,
        &mut block_import,
    );

    let b1 = client.header(&BlockId::Hash(b1)).unwrap().unwrap();

    // we should fail to import this block since the slot number didn't increase.
    // we will panic due to the `PanickingBlockImport` defined above.
    propose_and_import_block(
        &b1,
        Some(999.into()),
        &mut proposer_factory,
        &mut block_import,
    );
}

// TODO: Runtime at the moment doesn't implement transactions support, so root block extrinsic
//  verification fails in tests (`submit_test_store_root_block()` doesn't submit extrinsic as such).
// // Check that block import results in archiving working.
// #[test]
// fn archiving_works() {
//     let mut net = SubspaceTestNet::new(1);
//
//     let peer = net.peer(0);
//     let data = peer
//         .data
//         .as_ref()
//         .expect("Subspace link set up during initialization");
//     let client = peer
//         .client()
//         .as_client()
//         .clone();
//
//     let mut proposer_factory = DummyFactory {
//         client: client.clone(),
//         config: data.link.config.clone(),
//         epoch_changes: data.link.epoch_changes.clone(),
//         mutator: Arc::new(|_, _| ()),
//     };
//
//     let mut block_import = data
//         .block_import
//         .lock()
//         .take()
//         .expect("import set up during init");
//
//     let tokio_runtime = sc_cli::build_runtime().unwrap();
//     let task_manager = TaskManager::new(tokio_runtime.handle().clone(), None).unwrap();
//
//     super::start_subspace_archiver(&data.link, client.clone(), &task_manager.spawn_essential_handle());
//
//     let mut archived_segment_notification_stream =
//         data.link.archived_segment_notification_stream.subscribe();
//
//     let (archived_segment_sender, archived_segment_receiver) = mpsc::channel();
//
//     std::thread::spawn(move || {
//         tokio_runtime.block_on(async move {
//             while let Some(archived_segment_notification) =
//                 archived_segment_notification_stream.next().await
//             {
//                 archived_segment_sender
//                     .send(archived_segment_notification)
//                     .unwrap();
//             }
//         });
//     });
//
//     {
//         let mut parent_header = client.header(&BlockId::Number(0)).unwrap().unwrap();
//         for slot_number in 1..250 {
//             let block_hash = propose_and_import_block(
//                 &parent_header,
//                 Some(slot_number.into()),
//                 &mut proposer_factory,
//                 &mut block_import,
//             );
//
//             parent_header = client.header(&BlockId::Hash(block_hash)).unwrap().unwrap();
//         }
//     }
//
//     archived_segment_receiver.recv().unwrap();
// }
