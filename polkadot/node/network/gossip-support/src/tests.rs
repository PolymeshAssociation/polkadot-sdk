// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Unit tests for Gossip Support Subsystem.

use std::{collections::HashSet, sync::LazyLock, time::Duration};

use assert_matches::assert_matches;
use async_trait::async_trait;
use futures::{executor, future, Future};
use quickcheck::quickcheck;
use rand::seq::SliceRandom as _;

use parking_lot::Mutex;
use sc_network::multiaddr::Protocol;
use sp_authority_discovery::AuthorityPair as AuthorityDiscoveryPair;
use sp_consensus_babe::{AllowedSlots, BabeEpochConfiguration, Epoch as BabeEpoch};
use sp_core::crypto::Pair as PairT;
use sp_keyring::Sr25519Keyring;
use std::sync::Arc;

use polkadot_node_network_protocol::{
	grid_topology::{SessionGridTopology, TopologyPeerInfo},
	peer_set::ValidationVersion,
	ObservedRole,
};
use polkadot_node_subsystem::messages::{AllMessages, RuntimeApiMessage, RuntimeApiRequest};
use polkadot_node_subsystem_test_helpers as test_helpers;
use polkadot_node_subsystem_util::TimeoutExt as _;
use polkadot_primitives::{GroupIndex, IndexedVec};
use test_helpers::mock::{make_ferdie_keystore, new_leaf};

use super::*;

const AUTHORITY_KEYRINGS: &[Sr25519Keyring] = &[
	Sr25519Keyring::Alice,
	Sr25519Keyring::Bob,
	Sr25519Keyring::Charlie,
	Sr25519Keyring::Eve,
	Sr25519Keyring::One,
	Sr25519Keyring::Two,
	Sr25519Keyring::Ferdie,
];

static AUTHORITIES: LazyLock<Vec<AuthorityDiscoveryId>> =
	LazyLock::new(|| AUTHORITY_KEYRINGS.iter().map(|k| k.public().into()).collect());

static AUTHORITIES_WITHOUT_US: LazyLock<Vec<AuthorityDiscoveryId>> = LazyLock::new(|| {
	let mut a = AUTHORITIES.clone();
	a.pop(); // remove FERDIE.
	a
});

static PAST_PRESENT_FUTURE_AUTHORITIES: LazyLock<Vec<AuthorityDiscoveryId>> = LazyLock::new(|| {
	(0..50)
		.map(|_| AuthorityDiscoveryPair::generate().0.public())
		.chain(AUTHORITIES.clone())
		.collect()
});

static EXPECTED_SHUFFLING: LazyLock<Vec<usize>> = LazyLock::new(|| vec![6, 4, 0, 5, 2, 3, 1]);

static ROW_NEIGHBORS: LazyLock<Vec<ValidatorIndex>> =
	LazyLock::new(|| vec![ValidatorIndex::from(2)]);

static COLUMN_NEIGHBORS: LazyLock<Vec<ValidatorIndex>> =
	LazyLock::new(|| vec![ValidatorIndex::from(3), ValidatorIndex::from(5)]);

type VirtualOverseer =
	polkadot_node_subsystem_test_helpers::TestSubsystemContextHandle<GossipSupportMessage>;

#[derive(Debug, Clone)]
struct MockAuthorityDiscovery {
	addrs: Arc<Mutex<HashMap<AuthorityDiscoveryId, HashSet<Multiaddr>>>>,
	authorities: Arc<Mutex<HashMap<PeerId, HashSet<AuthorityDiscoveryId>>>>,
}

impl MockAuthorityDiscovery {
	fn new(authorities: Vec<AuthorityDiscoveryId>) -> Self {
		let authorities: HashMap<_, _> =
			authorities.clone().into_iter().map(|a| (PeerId::random(), a)).collect();
		let addrs = authorities
			.clone()
			.into_iter()
			.map(|(p, a)| {
				let multiaddr = Multiaddr::empty().with(Protocol::P2p(p.into()));
				(a, HashSet::from([multiaddr]))
			})
			.collect();
		Self {
			addrs: Arc::new(Mutex::new(addrs)),
			authorities: Arc::new(Mutex::new(
				authorities.into_iter().map(|(p, a)| (p, HashSet::from([a]))).collect(),
			)),
		}
	}

	fn change_address_for_authority(&self, authority_id: AuthorityDiscoveryId) -> PeerId {
		let new_peer_id = PeerId::random();
		let addr = Multiaddr::empty().with(Protocol::P2p(new_peer_id.into()));
		self.addrs.lock().insert(authority_id.clone(), HashSet::from([addr]));
		self.authorities.lock().insert(new_peer_id, HashSet::from([authority_id]));
		new_peer_id
	}

	fn authorities(&self) -> HashMap<PeerId, HashSet<AuthorityDiscoveryId>> {
		self.authorities.lock().clone()
	}

	fn add_more_authorities(
		&self,
		new_known: Vec<AuthorityDiscoveryId>,
	) -> HashMap<PeerId, HashSet<AuthorityDiscoveryId>> {
		let authorities: HashMap<_, _> =
			new_known.clone().into_iter().map(|a| (PeerId::random(), a)).collect();
		let addrs: HashMap<AuthorityDiscoveryId, HashSet<Multiaddr>> = authorities
			.clone()
			.into_iter()
			.map(|(p, a)| {
				let multiaddr = Multiaddr::empty().with(Protocol::P2p(p.into()));
				(a, HashSet::from([multiaddr]))
			})
			.collect();
		let authorities: HashMap<PeerId, HashSet<AuthorityDiscoveryId>> =
			authorities.into_iter().map(|(p, a)| (p, HashSet::from([a]))).collect();
		self.addrs.as_ref().lock().extend(addrs);
		self.authorities.as_ref().lock().extend(authorities.clone());
		authorities
	}
}

#[async_trait]
impl AuthorityDiscovery for MockAuthorityDiscovery {
	async fn get_addresses_by_authority_id(
		&mut self,
		authority: polkadot_primitives::AuthorityDiscoveryId,
	) -> Option<HashSet<sc_network::Multiaddr>> {
		self.addrs.lock().get(&authority).cloned()
	}

	async fn get_authority_ids_by_peer_id(
		&mut self,
		peer_id: polkadot_node_network_protocol::PeerId,
	) -> Option<HashSet<polkadot_primitives::AuthorityDiscoveryId>> {
		self.authorities.as_ref().lock().get(&peer_id).cloned()
	}
}

async fn get_multiaddrs(
	authorities: Vec<AuthorityDiscoveryId>,
	mock_authority_discovery: MockAuthorityDiscovery,
) -> Vec<HashSet<Multiaddr>> {
	let mut addrs = Vec::with_capacity(authorities.len());
	let mut discovery = mock_authority_discovery.clone();
	for authority in authorities.into_iter() {
		if let Some(addr) = discovery.get_addresses_by_authority_id(authority).await {
			addrs.push(addr);
		}
	}
	addrs
}

async fn get_address_map(
	authorities: Vec<AuthorityDiscoveryId>,
	mock_authority_discovery: MockAuthorityDiscovery,
) -> HashMap<AuthorityDiscoveryId, HashSet<Multiaddr>> {
	let mut addrs = HashMap::with_capacity(authorities.len());
	let mut discovery = mock_authority_discovery.clone();
	for authority in authorities.into_iter() {
		if let Some(addr) = discovery.get_addresses_by_authority_id(authority.clone()).await {
			addrs.insert(authority, addr);
		}
	}
	addrs
}

fn make_subsystem_with_authority_discovery(
	mock: MockAuthorityDiscovery,
) -> GossipSupport<MockAuthorityDiscovery> {
	GossipSupport::new(make_ferdie_keystore(), mock, Metrics::new_dummy())
}

fn test_harness<T: Future<Output = VirtualOverseer>, AD: AuthorityDiscovery>(
	subsystem: GossipSupport<AD>,
	test_fn: impl FnOnce(VirtualOverseer) -> T,
) -> GossipSupport<AD> {
	let pool = sp_core::testing::TaskExecutor::new();
	let (context, virtual_overseer) =
		polkadot_node_subsystem_test_helpers::make_subsystem_context(pool.clone());

	let subsystem = subsystem.run(context);

	let test_fut = test_fn(virtual_overseer);

	futures::pin_mut!(test_fut);
	futures::pin_mut!(subsystem);

	let (_, subsystem) = executor::block_on(future::join(
		async move {
			let mut overseer = test_fut.await;
			overseer
				.send(FromOrchestra::Signal(OverseerSignal::Conclude))
				.timeout(TIMEOUT)
				.await
				.expect("Conclude send timeout");
		},
		subsystem,
	));
	subsystem
}

const TIMEOUT: Duration = Duration::from_millis(100);

async fn overseer_signal_active_leaves(overseer: &mut VirtualOverseer, leaf: Hash) {
	let leaf = new_leaf(leaf, 0xdeadcafe);
	overseer
		.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(
			leaf,
		))))
		.timeout(TIMEOUT)
		.await
		.expect("signal send timeout");
}

fn make_session_info() -> SessionInfo {
	let all_validator_indices: Vec<_> = (0..6).map(ValidatorIndex::from).collect();
	SessionInfo {
		active_validator_indices: all_validator_indices.clone(),
		random_seed: [0; 32],
		dispute_period: 6,
		validators: AUTHORITY_KEYRINGS.iter().map(|k| k.public().into()).collect(),
		discovery_keys: AUTHORITIES.clone(),
		assignment_keys: AUTHORITY_KEYRINGS.iter().map(|k| k.public().into()).collect(),
		validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(vec![
			all_validator_indices,
		]),
		n_cores: 1,
		zeroth_delay_tranche_width: 1,
		relay_vrf_modulo_samples: 1,
		n_delay_tranches: 1,
		no_show_slots: 1,
		needed_approvals: 1,
	}
}

async fn overseer_recv(overseer: &mut VirtualOverseer) -> AllMessages {
	let msg = overseer.recv().timeout(TIMEOUT).await.expect("msg recv timeout");

	msg
}

async fn provide_info_for_finalized(overseer: &mut VirtualOverseer, test_session: SessionIndex) {
	assert_matches!(
		overseer_recv(overseer).await,
		AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(
			channel,
		)) => {
			channel.send(Ok(1)).unwrap();
		}
	);

	assert_matches!(
		overseer_recv(overseer).await,
		AllMessages::ChainApi(ChainApiMessage::FinalizedBlockHash(
			_,
			channel,
		)) => {
			channel.send(Ok(Some(Hash::repeat_byte(0xAA)))).unwrap();
		}
	);

	assert_matches!(
		overseer_recv(overseer).await,
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(
			_,
			RuntimeApiRequest::SessionIndexForChild(tx),
		)) => {
			// assert_eq!(relay_parent, hash);
			tx.send(Ok(test_session)).unwrap();
		}
	);
}

async fn test_neighbors(overseer: &mut VirtualOverseer, expected_session: SessionIndex) {
	assert_matches!(
		overseer_recv(overseer).await,
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(
			_,
			RuntimeApiRequest::CurrentBabeEpoch(tx),
		)) => {
			let _ = tx.send(Ok(BabeEpoch {
				epoch_index: 2 as _,
				start_slot: 0.into(),
				duration: 200,
				authorities: vec![(Sr25519Keyring::Alice.public().into(), 1)],
				randomness: [0u8; 32],
				config: BabeEpochConfiguration {
					c: (1, 4),
					allowed_slots: AllowedSlots::PrimarySlots,
				},
			})).unwrap();
		}
	);

	assert_matches!(
		overseer_recv(overseer).await,
		AllMessages::NetworkBridgeRx(NetworkBridgeRxMessage::NewGossipTopology {
			session: got_session,
			local_index,
			canonical_shuffling,
			shuffled_indices,
		}) => {
			assert_eq!(expected_session, got_session);
			assert_eq!(local_index, Some(ValidatorIndex(6)));
			assert_eq!(shuffled_indices, EXPECTED_SHUFFLING.clone());

			let grid_topology = SessionGridTopology::new(
				shuffled_indices,
				canonical_shuffling.into_iter()
					.map(|(a, v)| TopologyPeerInfo {
						validator_index: v,
						discovery_id: a,
						peer_ids: Vec::new(),
					})
					.collect(),
			);

			let grid_neighbors = grid_topology
				.compute_grid_neighbors_for(local_index.unwrap())
				.unwrap();

			let mut got_row: Vec<_> = grid_neighbors.validator_indices_x.into_iter().collect();
			let mut got_column: Vec<_> = grid_neighbors.validator_indices_y.into_iter().collect();
			got_row.sort();
			got_column.sort();
			assert_eq!(got_row, ROW_NEIGHBORS.clone());
			assert_eq!(got_column, COLUMN_NEIGHBORS.clone());
		}
	);
}

#[test]
fn issues_a_connection_request_on_new_session() {
	let mock_authority_discovery =
		MockAuthorityDiscovery::new(PAST_PRESENT_FUTURE_AUTHORITIES.clone());
	let mock_authority_discovery_clone = mock_authority_discovery.clone();
	let hash = Hash::repeat_byte(0xAA);
	let state = test_harness(
		make_subsystem_with_authority_discovery(mock_authority_discovery.clone()),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			overseer_signal_active_leaves(overseer, hash).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					tx.send(Ok(Some(make_session_info()))).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(AUTHORITIES.clone())).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
					validator_addrs,
					peer_set,
				}) => {
					assert_eq!(validator_addrs, get_multiaddrs(AUTHORITIES_WITHOUT_US.clone(), mock_authority_discovery_clone).await);
					assert_eq!(peer_set, PeerSet::Validation);
				}
			);
			provide_info_for_finalized(overseer, 1).await;

			test_neighbors(overseer, 1).await;

			virtual_overseer
		},
	);

	assert_eq!(state.last_session_index, Some(1));
	assert!(state.last_failure.is_none());

	// does not issue on the same session
	let hash = Hash::repeat_byte(0xBB);
	let state = test_harness(state, |mut virtual_overseer| async move {
		let overseer = &mut virtual_overseer;
		overseer_signal_active_leaves(overseer, hash).await;
		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::SessionIndexForChild(tx),
			)) => {
				assert_eq!(relay_parent, hash);
				tx.send(Ok(1)).unwrap();
			}
		);

		virtual_overseer
	});

	assert_eq!(state.last_session_index, Some(1));
	assert!(state.last_failure.is_none());

	// does on the new one
	let hash = Hash::repeat_byte(0xCC);
	let state = test_harness(state, |mut virtual_overseer| async move {
		let overseer = &mut virtual_overseer;
		overseer_signal_active_leaves(overseer, hash).await;
		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::SessionIndexForChild(tx),
			)) => {
				assert_eq!(relay_parent, hash);
				tx.send(Ok(2)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::SessionInfo(s, tx),
			)) => {
				assert_eq!(relay_parent, hash);
				assert_eq!(s, 2);
				tx.send(Ok(Some(make_session_info()))).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::Authorities(tx),
			)) => {
				assert_eq!(relay_parent, hash);
				tx.send(Ok(AUTHORITIES.clone())).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
				validator_addrs,
				peer_set,
			}) => {
				assert_eq!(validator_addrs, get_multiaddrs(AUTHORITIES_WITHOUT_US.clone(), mock_authority_discovery.clone()).await);
				assert_eq!(peer_set, PeerSet::Validation);
			}
		);

		test_neighbors(overseer, 2).await;

		virtual_overseer
	});
	assert_eq!(state.last_session_index, Some(2));
	assert!(state.last_failure.is_none());
}

#[test]
fn issues_connection_request_to_past_present_future() {
	let hash = Hash::repeat_byte(0xAA);
	let mock_authority_discovery =
		MockAuthorityDiscovery::new(PAST_PRESENT_FUTURE_AUTHORITIES.clone());
	test_harness(
		make_subsystem_with_authority_discovery(mock_authority_discovery.clone()),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			overseer_signal_active_leaves(overseer, hash).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					tx.send(Ok(Some(make_session_info()))).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(PAST_PRESENT_FUTURE_AUTHORITIES.clone())).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
					validator_addrs,
					peer_set,
				}) => {
					let all_without_ferdie: Vec<_> = PAST_PRESENT_FUTURE_AUTHORITIES
						.iter()
						.cloned()
						.filter(|p| p != &Sr25519Keyring::Ferdie.public().into())
						.collect();

					let addrs = get_multiaddrs(all_without_ferdie, mock_authority_discovery.clone()).await;

					assert_eq!(validator_addrs, addrs);
					assert_eq!(peer_set, PeerSet::Validation);
				}
			);
			provide_info_for_finalized(overseer, 1).await;

			// Ensure neighbors are unaffected
			test_neighbors(overseer, 1).await;

			virtual_overseer
		},
	);
}

// Test we notify peer about learning of the authority ID after session boundary, when we couldn't
// connect to more than 1/3 of the authorities.
#[test]
fn issues_update_authorities_after_session() {
	let hash = Hash::repeat_byte(0xAA);

	let mut authorities = PAST_PRESENT_FUTURE_AUTHORITIES.clone();
	let unknown_at_session = authorities.split_off(authorities.len() / 3 - 1);
	let mut authority_discovery_mock = MockAuthorityDiscovery::new(authorities);

	test_harness(
		make_subsystem_with_authority_discovery(authority_discovery_mock.clone()),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// 1. Initialize with the first leaf in the session.
			overseer_signal_active_leaves(overseer, hash).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					let mut session_info = make_session_info();
					session_info.discovery_keys = PAST_PRESENT_FUTURE_AUTHORITIES.clone();
					tx.send(Ok(Some(session_info))).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(PAST_PRESENT_FUTURE_AUTHORITIES.clone())).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
					validator_addrs,
					peer_set,
				}) => {
					let all_without_ferdie: Vec<_> = PAST_PRESENT_FUTURE_AUTHORITIES
						.iter()
						.cloned()
						.filter(|p| p != &Sr25519Keyring::Ferdie.public().into())
						.collect();

					let addrs = get_multiaddrs(all_without_ferdie, authority_discovery_mock.clone()).await;

					assert_eq!(validator_addrs, addrs);
					assert_eq!(peer_set, PeerSet::Validation);
				}
			);

			provide_info_for_finalized(overseer, 1).await;
			// Ensure neighbors are unaffected
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::CurrentBabeEpoch(tx),
				)) => {
					let _ = tx.send(Ok(BabeEpoch {
						epoch_index: 2 as _,
						start_slot: 0.into(),
						duration: 200,
						authorities: vec![(Sr25519Keyring::Alice.public().into(), 1)],
						randomness: [0u8; 32],
						config: BabeEpochConfiguration {
							c: (1, 4),
							allowed_slots: AllowedSlots::PrimarySlots,
						},
					})).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeRx(NetworkBridgeRxMessage::NewGossipTopology {
					session: _,
					local_index: _,
					canonical_shuffling: _,
					shuffled_indices: _,
				}) => {

				}
			);

			// 2. Connect all authorities that are known so far.
			let known_authorities = authority_discovery_mock.authorities();
			for (peer_id, _id) in known_authorities.iter() {
				let msg =
					GossipSupportMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
						*peer_id,
						ObservedRole::Authority,
						ValidationVersion::V3.into(),
						None,
					));
				overseer.send(FromOrchestra::Communication { msg }).await
			}

			Delay::new(BACKOFF_DURATION).await;
			// 3. Send a new leaf after BACKOFF_DURATION  and check UpdateAuthority is emitted for
			//    all known connected peers.
			let hash = Hash::repeat_byte(0xBB);
			overseer_signal_active_leaves(overseer, hash).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					let mut session_info = make_session_info();
					session_info.discovery_keys = PAST_PRESENT_FUTURE_AUTHORITIES.clone();
					tx.send(Ok(Some(session_info))).unwrap();

				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(PAST_PRESENT_FUTURE_AUTHORITIES.clone())).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
					validator_addrs: _,
					peer_set: _,
				}) => {
				}
			);

			for _ in 0..known_authorities.len() {
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeRx(NetworkBridgeRxMessage::UpdatedAuthorityIds {
						peer_id,
						authority_ids,
					}) => {
						assert_eq!(authority_discovery_mock.get_authority_ids_by_peer_id(peer_id).await.unwrap_or_default(), authority_ids);
					}
				);
			}

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none());
			// 4. Connect more authorities except one
			let newly_added = authority_discovery_mock.add_more_authorities(unknown_at_session);
			let mut newly_added_iter = newly_added.iter();
			let unconnected_at_last_retry = newly_added_iter
				.next()
				.map(|(peer_id, authority_id)| (*peer_id, authority_id.clone()))
				.unwrap();
			for (peer_id, _) in newly_added_iter {
				let msg =
					GossipSupportMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
						*peer_id,
						ObservedRole::Authority,
						ValidationVersion::V3.into(),
						None,
					));
				overseer.send(FromOrchestra::Communication { msg }).await
			}

			// 5. Send a new leaf and check UpdateAuthority is emitted only for the newly connected
			//    peers.
			let hash = Hash::repeat_byte(0xCC);
			Delay::new(BACKOFF_DURATION).await;
			overseer_signal_active_leaves(overseer, hash).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					let mut session_info = make_session_info();
					session_info.discovery_keys = PAST_PRESENT_FUTURE_AUTHORITIES.clone();
					tx.send(Ok(Some(session_info))).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(PAST_PRESENT_FUTURE_AUTHORITIES.clone())).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
					validator_addrs: _,
					peer_set: _,
				}) => {
				}
			);

			for _ in 1..newly_added.len() {
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeRx(NetworkBridgeRxMessage::UpdatedAuthorityIds {
						peer_id,
						authority_ids,
					}) => {
						assert_ne!(peer_id, unconnected_at_last_retry.0);
						assert_eq!(newly_added.get(&peer_id).cloned().unwrap_or_default(), authority_ids);
					}
				);
			}

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none());
			virtual_overseer
		},
	);
}

// Test we connect to authorities that changed their address `TRY_RERESOLVE_AUTHORITIES` rate
// and that is is no-op if no authority changed.
#[test]
fn test_quickly_connect_to_authorities_that_changed_address() {
	let hash = Hash::repeat_byte(0xAA);

	let authorities = PAST_PRESENT_FUTURE_AUTHORITIES.clone();
	let authority_that_changes_address = authorities.get(5).unwrap().clone();

	let mut authority_discovery_mock = MockAuthorityDiscovery::new(authorities);

	test_harness(
		make_subsystem_with_authority_discovery(authority_discovery_mock.clone()),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// 1. Initialize with the first leaf in the session.
			overseer_signal_active_leaves(overseer, hash).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					let mut session_info = make_session_info();
					session_info.discovery_keys = PAST_PRESENT_FUTURE_AUTHORITIES.clone();
					tx.send(Ok(Some(session_info))).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(PAST_PRESENT_FUTURE_AUTHORITIES.clone())).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
					validator_addrs,
					peer_set,
				}) => {
					let all_without_ferdie: Vec<_> = PAST_PRESENT_FUTURE_AUTHORITIES
						.iter()
						.cloned()
						.filter(|p| p != &Sr25519Keyring::Ferdie.public().into())
						.collect();

					let addrs = get_multiaddrs(all_without_ferdie, authority_discovery_mock.clone()).await;

					assert_eq!(validator_addrs, addrs);
					assert_eq!(peer_set, PeerSet::Validation);
				}
			);

			provide_info_for_finalized(overseer, 1).await;
			// Ensure neighbors are unaffected
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::CurrentBabeEpoch(tx),
				)) => {
					let _ = tx.send(Ok(BabeEpoch {
						epoch_index: 2 as _,
						start_slot: 0.into(),
						duration: 200,
						authorities: vec![(Sr25519Keyring::Alice.public().into(), 1)],
						randomness: [0u8; 32],
						config: BabeEpochConfiguration {
							c: (1, 4),
							allowed_slots: AllowedSlots::PrimarySlots,
						},
					})).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeRx(NetworkBridgeRxMessage::NewGossipTopology {
					session: _,
					local_index: _,
					canonical_shuffling: _,
					shuffled_indices: _,
				}) => {

				}
			);

			// 2. Connect all authorities that are known so far.
			let known_authorities = authority_discovery_mock.authorities();
			for (peer_id, _id) in known_authorities.iter() {
				let msg =
					GossipSupportMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
						*peer_id,
						ObservedRole::Authority,
						ValidationVersion::V3.into(),
						None,
					));
				overseer.send(FromOrchestra::Communication { msg }).await
			}

			// 3. Send a new leaf after TRY_RERESOLVE_AUTHORITIES, we should notice
			//    UpdateAuthorithies is emitted for all ConnectedPeers.
			Delay::new(TRY_RERESOLVE_AUTHORITIES).await;
			let hash = Hash::repeat_byte(0xBB);
			overseer_signal_active_leaves(overseer, hash).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					let mut session_info = make_session_info();
					session_info.discovery_keys = PAST_PRESENT_FUTURE_AUTHORITIES.clone();
					tx.send(Ok(Some(session_info))).unwrap();

				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(PAST_PRESENT_FUTURE_AUTHORITIES.clone())).unwrap();
				}
			);

			for _ in 0..known_authorities.len() {
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeRx(NetworkBridgeRxMessage::UpdatedAuthorityIds {
						peer_id,
						authority_ids,
					}) => {
						assert_eq!(authority_discovery_mock.get_authority_ids_by_peer_id(peer_id).await.unwrap_or_default(), authority_ids);
					}
				);
			}

			// 4. At next re-resolve no-authorithy changes their address, so it should be no-op.
			Delay::new(TRY_RERESOLVE_AUTHORITIES).await;
			let hash = Hash::repeat_byte(0xCC);
			overseer_signal_active_leaves(overseer, hash).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					let mut session_info = make_session_info();
					session_info.discovery_keys = PAST_PRESENT_FUTURE_AUTHORITIES.clone();
					tx.send(Ok(Some(session_info))).unwrap();

				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(PAST_PRESENT_FUTURE_AUTHORITIES.clone())).unwrap();
				}
			);
			assert!(overseer.recv().timeout(TIMEOUT).await.is_none());

			// Change address for one authorithy and check we try to connect to it and
			// that we emit UpdateAuthorityID for the old PeerId and the new one.
			Delay::new(TRY_RERESOLVE_AUTHORITIES).await;
			let changed_peerid = authority_discovery_mock
				.change_address_for_authority(authority_that_changes_address.clone());
			let hash = Hash::repeat_byte(0xDD);
			let msg = GossipSupportMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
				changed_peerid,
				ObservedRole::Authority,
				ValidationVersion::V3.into(),
				None,
			));
			overseer.send(FromOrchestra::Communication { msg }).await;

			overseer_signal_active_leaves(overseer, hash).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					let mut session_info = make_session_info();
					session_info.discovery_keys = PAST_PRESENT_FUTURE_AUTHORITIES.clone();
					tx.send(Ok(Some(session_info))).unwrap();

				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(PAST_PRESENT_FUTURE_AUTHORITIES.clone())).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::AddToResolvedValidators {
					validator_addrs,
					peer_set,
				}) => {
					let expected = get_address_map(vec![authority_that_changes_address.clone()], authority_discovery_mock.clone()).await;
					let expected: HashSet<Multiaddr> = expected.into_values().flat_map(|v| v.into_iter()).collect();
					assert_eq!(validator_addrs.into_iter().flat_map(|v| v.into_iter()).collect::<HashSet<_>>(), expected);
					assert_eq!(peer_set, PeerSet::Validation);
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeRx(NetworkBridgeRxMessage::UpdatedAuthorityIds {
					peer_id,
					authority_ids,
				}) => {
					assert_eq!(authority_discovery_mock.get_authority_ids_by_peer_id(peer_id).await.unwrap(), HashSet::from([authority_that_changes_address.clone()]));
					assert!(authority_ids.is_empty());
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeRx(NetworkBridgeRxMessage::UpdatedAuthorityIds {
					peer_id,
					authority_ids,
				}) => {
					assert_eq!(authority_ids, HashSet::from([authority_that_changes_address]));
					assert_eq!(changed_peerid, peer_id);
				}
			);

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none());

			virtual_overseer
		},
	);
}

#[test]
fn disconnect_when_not_in_past_present_future() {
	sp_tracing::try_init_simple();
	let mock_authority_discovery =
		MockAuthorityDiscovery::new(PAST_PRESENT_FUTURE_AUTHORITIES.clone());
	let hash = Hash::repeat_byte(0xAA);
	test_harness(
		make_subsystem_with_authority_discovery(mock_authority_discovery.clone()),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			overseer_signal_active_leaves(overseer, hash).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					let mut heute_leider_nicht = make_session_info();
					heute_leider_nicht.discovery_keys = AUTHORITIES_WITHOUT_US.clone();
					tx.send(Ok(Some(heute_leider_nicht))).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(AUTHORITIES_WITHOUT_US.clone())).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
					validator_addrs,
					peer_set,
				}) => {
					assert!(validator_addrs.is_empty());
					assert_eq!(peer_set, PeerSet::Validation);
				}
			);

			provide_info_for_finalized(overseer, 1).await;
			virtual_overseer
		},
	);
}

#[test]
fn test_log_output() {
	sp_tracing::try_init_simple();
	let alice: AuthorityDiscoveryId = Sr25519Keyring::Alice.public().into();
	let bob = Sr25519Keyring::Bob.public().into();
	let unconnected_authorities = {
		let mut m = HashMap::new();
		let peer_id = PeerId::random();
		let addr = Multiaddr::empty().with(Protocol::P2p(peer_id.into()));
		let addrs = HashSet::from([addr.clone(), addr]);
		m.insert(alice, addrs);
		let peer_id = PeerId::random();
		let addr = Multiaddr::empty().with(Protocol::P2p(peer_id.into()));
		let addrs = HashSet::from([addr.clone(), addr]);
		m.insert(bob, addrs);
		m
	};
	let pretty = PrettyAuthorities(unconnected_authorities.iter());
	gum::debug!(
		target: LOG_TARGET,
		unconnected_authorities = %pretty,
		"Connectivity Report"
	);
}

#[test]
fn issues_a_connection_request_when_last_request_was_mostly_unresolved() {
	let hash = Hash::repeat_byte(0xAA);
	let mock_authority_discovery =
		MockAuthorityDiscovery::new(PAST_PRESENT_FUTURE_AUTHORITIES.clone());
	let state = make_subsystem_with_authority_discovery(mock_authority_discovery.clone());
	// There will be two lookup failures:
	let alice = Sr25519Keyring::Alice.public().into();
	let bob = Sr25519Keyring::Bob.public().into();
	let alice_addr = state.authority_discovery.addrs.lock().remove(&alice);
	state.authority_discovery.addrs.lock().remove(&bob);
	let mock_authority_discovery_clone = mock_authority_discovery.clone();
	let mut state = {
		let alice = alice.clone();
		let bob = bob.clone();

		test_harness(state, |mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			overseer_signal_active_leaves(overseer, hash).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(1)).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					tx.send(Ok(Some(make_session_info()))).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(AUTHORITIES.clone())).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
					validator_addrs,
					peer_set,
				}) => {
					let mut expected = get_address_map(AUTHORITIES_WITHOUT_US.clone(), mock_authority_discovery_clone.clone()).await;
					expected.remove(&alice);
					expected.remove(&bob);
					let expected: HashSet<Multiaddr> = expected.into_values().flat_map(|v| v.into_iter()).collect();
					assert_eq!(validator_addrs.into_iter().flat_map(|v| v.into_iter()).collect::<HashSet<_>>(), expected);
					assert_eq!(peer_set, PeerSet::Validation);
				}
			);
			provide_info_for_finalized(overseer, 1).await;

			test_neighbors(overseer, 1).await;

			virtual_overseer
		})
	};
	assert_eq!(state.last_session_index, Some(1));
	assert!(state.last_failure.is_some());
	state.last_failure = state.last_failure.and_then(|i| i.checked_sub(BACKOFF_DURATION));
	// One error less:
	state.authority_discovery.addrs.lock().insert(alice, alice_addr.unwrap());

	let hash = Hash::repeat_byte(0xBB);
	let state = test_harness(state, |mut virtual_overseer| async move {
		let overseer = &mut virtual_overseer;
		overseer_signal_active_leaves(overseer, hash).await;
		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::SessionIndexForChild(tx),
			)) => {
				assert_eq!(relay_parent, hash);
				tx.send(Ok(1)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::SessionInfo(s, tx),
			)) => {
				assert_eq!(relay_parent, hash);
				assert_eq!(s, 1);
				tx.send(Ok(Some(make_session_info()))).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::Authorities(tx),
			)) => {
				assert_eq!(relay_parent, hash);
				tx.send(Ok(AUTHORITIES.clone())).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(overseer).await,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
				validator_addrs,
				peer_set,
			}) => {
				let mut expected = get_address_map(AUTHORITIES_WITHOUT_US.clone(), mock_authority_discovery.clone()).await;
				expected.remove(&bob);
				let expected: HashSet<Multiaddr> = expected.into_values().flat_map(|v| v.into_iter()).collect();
				assert_eq!(validator_addrs.into_iter().flat_map(|v| v.into_iter()).collect::<HashSet<_>>(), expected);
				assert_eq!(peer_set, PeerSet::Validation);
			}
		);

		virtual_overseer
	});

	assert_eq!(state.last_session_index, Some(1));
	assert!(state.last_failure.is_none());
}

// Test that topology is updated for all sessions we still have unfinalized blocks for.
#[test]
fn updates_topology_for_all_finalized_blocks() {
	let hash = Hash::repeat_byte(0xAA);
	let mock_authority_discovery =
		MockAuthorityDiscovery::new(PAST_PRESENT_FUTURE_AUTHORITIES.clone());
	test_harness(
		make_subsystem_with_authority_discovery(mock_authority_discovery.clone()),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			overseer_signal_active_leaves(overseer, hash).await;
			let active_session = 5;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(active_session)).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, active_session);
					tx.send(Ok(Some(make_session_info()))).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Authorities(tx),
				)) => {
					assert_eq!(relay_parent, hash);
					tx.send(Ok(PAST_PRESENT_FUTURE_AUTHORITIES.clone())).unwrap();
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToResolvedValidators {
					validator_addrs,
					peer_set,
				}) => {
					let all_without_ferdie: Vec<_> = PAST_PRESENT_FUTURE_AUTHORITIES
						.iter()
						.cloned()
						.filter(|p| p != &Sr25519Keyring::Ferdie.public().into())
						.collect();

					let addrs = get_multiaddrs(all_without_ferdie, mock_authority_discovery.clone()).await;

					assert_eq!(validator_addrs, addrs);
					assert_eq!(peer_set, PeerSet::Validation);
				}
			);

			// Ensure first time we update the topology we also update topology for the session last
			// finalized is in.
			provide_info_for_finalized(overseer, 1).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::SessionInfo(s, tx),
				)) => {
					assert_eq!(relay_parent, hash);
					assert_eq!(s, 1);
					tx.send(Ok(Some(make_session_info()))).unwrap();
				}
			);
			// Ensure  we received topology for the session last finalized is in and the current
			// active session
			test_neighbors(overseer, 1).await;
			test_neighbors(overseer, active_session).await;

			let mut block_number = 3;
			// As finalized progresses, we should update topology for all sessions until we caught
			// up with the known sessions.
			for finalized in 2..active_session {
				block_number += 1;
				overseer
					.send(FromOrchestra::Signal(OverseerSignal::BlockFinalized(
						Hash::repeat_byte(block_number as u8),
						block_number,
					)))
					.timeout(TIMEOUT)
					.await
					.expect("signal send timeout");
				provide_info_for_finalized(overseer, finalized).await;
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						relay_parent,
						RuntimeApiRequest::SessionInfo(s, tx),
					)) => {
						assert_eq!(relay_parent, hash);
						assert_eq!(s, finalized);
						tx.send(Ok(Some(make_session_info()))).unwrap();
					}
				);
				test_neighbors(overseer, finalized).await;

				block_number += 1;
				overseer
					.send(FromOrchestra::Signal(OverseerSignal::BlockFinalized(
						Hash::repeat_byte(block_number as u8),
						block_number,
					)))
					.timeout(TIMEOUT)
					.await
					.expect("signal send timeout");
				provide_info_for_finalized(overseer, finalized).await;
			}

			// No topology update is sent once finalized block is in the active session.
			block_number += 1;
			overseer
				.send(FromOrchestra::Signal(OverseerSignal::BlockFinalized(
					Hash::repeat_byte(block_number as u8),
					block_number,
				)))
				.timeout(TIMEOUT)
				.await
				.expect("signal send timeout");
			provide_info_for_finalized(overseer, active_session).await;

			// Code becomes no-op after we caught up with the last finalized block being in the
			// active session.
			block_number += 1;
			overseer
				.send(FromOrchestra::Signal(OverseerSignal::BlockFinalized(
					Hash::repeat_byte(block_number as u8),
					block_number,
				)))
				.timeout(TIMEOUT)
				.await
				.expect("signal send timeout");

			virtual_overseer
		},
	);
}

// note: this test was added at a time where the default `rand::SliceRandom::shuffle`
// function was used to shuffle authorities for the topology and ensures backwards compatibility.
//
// in the same commit, an explicit fisher-yates implementation was added in place of the unspecified
// behavior of that function. If this test begins to fail at some point in the future, it can simply
// be removed as the desired behavior has been preserved.
quickcheck! {
	fn rng_shuffle_equals_fisher_yates(x: Vec<i32>, seed_base: u8) -> bool {
		let mut rng1: ChaCha20Rng = SeedableRng::from_seed([seed_base; 32]);
		let mut rng2: ChaCha20Rng = SeedableRng::from_seed([seed_base; 32]);

		let mut data1 = x.clone();
		let mut data2 = x;

		data1.shuffle(&mut rng1);
		crate::fisher_yates_shuffle(&mut rng2, &mut data2[..]);
		data1 == data2
	}
}
