// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
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

//! `SyncingEngine` is the actor responsible for syncing Substrate chain
//! to tip and keep the blockchain up to date with network updates.

use crate::{
	block_announce_validator::{
		BlockAnnounceValidationResult, BlockAnnounceValidator as BlockAnnounceValidatorStream,
	},
	service::{self, chain_sync::ToServiceCommand},
	warp::WarpSyncParams,
	ChainSync, ClientError, SyncingService,
};

use codec::{Decode, Encode};
use futures::{
	channel::oneshot,
	future::{BoxFuture, Fuse},
	FutureExt, StreamExt,
};
use futures_timer::Delay;
use libp2p::PeerId;
use prometheus_endpoint::{
	register, Gauge, GaugeVec, MetricSource, Opts, PrometheusError, Registry, SourcedGauge, U64,
};
use schnellru::{ByLength, LruMap};

use sc_client_api::{BlockBackend, HeaderBackend, ProofProvider};
use sc_consensus::import_queue::ImportQueueService;
use sc_network::{
	config::{FullNetworkConfiguration, NonDefaultSetConfig, ProtocolId},
	utils::LruHashSet,
	NotificationsSink, ProtocolName, ReputationChange,
};
use sc_network_common::{
	role::Roles,
	sync::{
		message::{BlockAnnounce, BlockAnnouncesHandshake, BlockState},
		BadPeer, ChainSync as ChainSyncT, ExtendedPeerInfo, SyncEvent,
	},
};
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};
use sp_blockchain::HeaderMetadata;
use sp_consensus::block_validation::BlockAnnounceValidator;
use sp_runtime::traits::{Block as BlockT, Header, NumberFor, Zero};

use std::{
	collections::{HashMap, HashSet},
	num::NonZeroUsize,
	sync::{
		atomic::{AtomicBool, AtomicUsize, Ordering},
		Arc,
	},
	task::Poll,
	time::{Duration, Instant},
};

/// Log target for this file.
const LOG_TARGET: &'static str = "sync";

/// Interval at which we perform time based maintenance
const TICK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1100);

/// Maximum number of known block hashes to keep for a peer.
const MAX_KNOWN_BLOCKS: usize = 1024; // ~32kb per peer + LruHashSet overhead

/// If the block announces stream to peer has been inactive for 30 seconds meaning local node
/// has not sent or received block announcements to/from the peer, report the node for inactivity,
/// disconnect it and attempt to establish connection to some other peer.
const INACTIVITY_EVICT_THRESHOLD: Duration = Duration::from_secs(30);

/// When `SyncingEngine` is started, wait two minutes before actually staring to count peers as
/// evicted.
///
/// Parachain collator may incorrectly get evicted because it's waiting to receive a number of
/// relaychain blocks before it can start creating parachain blocks. During this wait,
/// `SyncingEngine` still counts it as active and as the peer is not sending blocks, it may get
/// evicted if a block is not received within the first 30 secons since the peer connected.
///
/// To prevent this from happening, define a threshold for how long `SyncingEngine` should wait
/// before it starts evicting peers.
const INITIAL_EVICTION_WAIT_PERIOD: Duration = Duration::from_secs(2 * 60);

mod rep {
	use sc_network::ReputationChange as Rep;
	/// Peer has different genesis.
	pub const GENESIS_MISMATCH: Rep = Rep::new_fatal("Genesis mismatch");
	/// Peer send us a block announcement that failed at validation.
	pub const BAD_BLOCK_ANNOUNCEMENT: Rep = Rep::new(-(1 << 12), "Bad block announcement");
	/// Block announce substream with the peer has been inactive too long
	pub const INACTIVE_SUBSTREAM: Rep = Rep::new(-(1 << 10), "Inactive block announce substream");
}

struct Metrics {
	peers: Gauge<U64>,
	queued_blocks: Gauge<U64>,
	fork_targets: Gauge<U64>,
	justifications: GaugeVec<U64>,
}

impl Metrics {
	fn register(r: &Registry, major_syncing: Arc<AtomicBool>) -> Result<Self, PrometheusError> {
		let _ = MajorSyncingGauge::register(r, major_syncing)?;
		Ok(Self {
			peers: {
				let g = Gauge::new("substrate_sync_peers", "Number of peers we sync with")?;
				register(g, r)?
			},
			queued_blocks: {
				let g =
					Gauge::new("substrate_sync_queued_blocks", "Number of blocks in import queue")?;
				register(g, r)?
			},
			fork_targets: {
				let g = Gauge::new("substrate_sync_fork_targets", "Number of fork sync targets")?;
				register(g, r)?
			},
			justifications: {
				let g = GaugeVec::new(
					Opts::new(
						"substrate_sync_extra_justifications",
						"Number of extra justifications requests",
					),
					&["status"],
				)?;
				register(g, r)?
			},
		})
	}
}

/// The "major syncing" metric.
#[derive(Clone)]
pub struct MajorSyncingGauge(Arc<AtomicBool>);

impl MajorSyncingGauge {
	/// Registers the [`MajorSyncGauge`] metric whose value is
	/// obtained from the given `AtomicBool`.
	fn register(registry: &Registry, value: Arc<AtomicBool>) -> Result<(), PrometheusError> {
		prometheus_endpoint::register(
			SourcedGauge::new(
				&Opts::new(
					"substrate_sub_libp2p_is_major_syncing",
					"Whether the node is performing a major sync or not.",
				),
				MajorSyncingGauge(value),
			)?,
			registry,
		)?;

		Ok(())
	}
}

impl MetricSource for MajorSyncingGauge {
	type N = u64;

	fn collect(&self, mut set: impl FnMut(&[&str], Self::N)) {
		set(&[], self.0.load(Ordering::Relaxed) as u64);
	}
}

/// Peer information
#[derive(Debug)]
pub struct Peer<B: BlockT> {
	pub info: ExtendedPeerInfo<B>,
	/// Holds a set of blocks known to this peer.
	pub known_blocks: LruHashSet<B::Hash>,
	/// Notification sink.
	sink: NotificationsSink,
	/// Is the peer inbound.
	inbound: bool,
}

pub struct SyncingEngine<B: BlockT, Client> {
	/// State machine that handles the list of in-progress requests. Only full node peers are
	/// registered.
	chain_sync: ChainSync<B, Client>,

	/// Blockchain client.
	client: Arc<Client>,

	/// Number of peers we're connected to.
	num_connected: Arc<AtomicUsize>,

	/// Are we actively catching up with the chain?
	is_major_syncing: Arc<AtomicBool>,

	/// Network service.
	network_service: service::network::NetworkServiceHandle,

	/// Channel for receiving service commands
	service_rx: TracingUnboundedReceiver<ToServiceCommand<B>>,

	/// Channel for receiving inbound connections from `Protocol`.
	rx: sc_utils::mpsc::TracingUnboundedReceiver<sc_network::SyncEvent<B>>,

	/// Assigned roles.
	roles: Roles,

	/// Genesis hash.
	genesis_hash: B::Hash,

	/// Set of channels for other protocols that have subscribed to syncing events.
	event_streams: Vec<TracingUnboundedSender<SyncEvent>>,

	/// Interval at which we call `tick`.
	tick_timeout: Delay,

	/// All connected peers. Contains both full and light node peers.
	peers: HashMap<PeerId, Peer<B>>,

	/// List of nodes for which we perform additional logging because they are important for the
	/// user.
	important_peers: HashSet<PeerId>,

	/// Actual list of connected no-slot nodes.
	default_peers_set_no_slot_connected_peers: HashSet<PeerId>,

	/// List of nodes that should never occupy peer slots.
	default_peers_set_no_slot_peers: HashSet<PeerId>,

	/// Value that was passed as part of the configuration. Used to cap the number of full
	/// nodes.
	default_peers_set_num_full: usize,

	/// Number of slots to allocate to light nodes.
	default_peers_set_num_light: usize,

	/// Maximum number of inbound peers.
	max_in_peers: usize,

	/// Number of inbound peers accepted so far.
	num_in_peers: usize,

	/// Async processor of block announce validations.
	block_announce_validator: BlockAnnounceValidatorStream<B>,

	/// A cache for the data that was associated to a block announcement.
	block_announce_data_cache: LruMap<B::Hash, Vec<u8>>,

	/// The `PeerId`'s of all boot nodes.
	boot_node_ids: HashSet<PeerId>,

	/// A channel to get target block header if we skip over proofs downloading during warp sync.
	warp_sync_target_block_header_rx:
		Fuse<BoxFuture<'static, Result<B::Header, oneshot::Canceled>>>,

	/// Protocol name used for block announcements
	block_announce_protocol_name: ProtocolName,

	/// Prometheus metrics.
	metrics: Option<Metrics>,

	/// When the syncing was started.
	///
	/// Stored as an `Option<Instant>` so once the initial wait has passed, `SyncingEngine`
	/// can reset the peer timers and continue with the normal eviction process.
	syncing_started: Option<Instant>,

	/// Instant when the last notification was sent or received.
	last_notification_io: Instant,
}

impl<B: BlockT, Client> SyncingEngine<B, Client>
where
	B: BlockT,
	Client: HeaderBackend<B>
		+ BlockBackend<B>
		+ HeaderMetadata<B, Error = sp_blockchain::Error>
		+ ProofProvider<B>
		+ Send
		+ Sync
		+ 'static,
{
	pub fn new(
		roles: Roles,
		client: Arc<Client>,
		metrics_registry: Option<&Registry>,
		net_config: &FullNetworkConfiguration,
		protocol_id: ProtocolId,
		fork_id: &Option<String>,
		block_announce_validator: Box<dyn BlockAnnounceValidator<B> + Send>,
		warp_sync_params: Option<WarpSyncParams<B>>,
		network_service: service::network::NetworkServiceHandle,
		import_queue: Box<dyn ImportQueueService<B>>,
		block_request_protocol_name: ProtocolName,
		state_request_protocol_name: ProtocolName,
		warp_sync_protocol_name: Option<ProtocolName>,
		rx: sc_utils::mpsc::TracingUnboundedReceiver<sc_network::SyncEvent<B>>,
	) -> Result<(Self, SyncingService<B>, NonDefaultSetConfig), ClientError> {
		let mode = net_config.network_config.sync_mode;
		let max_parallel_downloads = net_config.network_config.max_parallel_downloads;
		let max_blocks_per_request = if net_config.network_config.max_blocks_per_request >
			crate::MAX_BLOCKS_IN_RESPONSE as u32
		{
			log::info!(
				target: LOG_TARGET,
				"clamping maximum blocks per request to {}",
				crate::MAX_BLOCKS_IN_RESPONSE,
			);
			crate::MAX_BLOCKS_IN_RESPONSE as u32
		} else {
			net_config.network_config.max_blocks_per_request
		};
		let cache_capacity = (net_config.network_config.default_peers_set.in_peers +
			net_config.network_config.default_peers_set.out_peers)
			.max(1);
		let important_peers = {
			let mut imp_p = HashSet::new();
			for reserved in &net_config.network_config.default_peers_set.reserved_nodes {
				imp_p.insert(reserved.peer_id);
			}
			for config in net_config.notification_protocols() {
				let peer_ids = config
					.set_config
					.reserved_nodes
					.iter()
					.map(|info| info.peer_id)
					.collect::<Vec<PeerId>>();
				imp_p.extend(peer_ids);
			}

			imp_p.shrink_to_fit();
			imp_p
		};
		let boot_node_ids = {
			let mut list = HashSet::new();
			for node in &net_config.network_config.boot_nodes {
				list.insert(node.peer_id);
			}
			list.shrink_to_fit();
			list
		};
		let default_peers_set_no_slot_peers = {
			let mut no_slot_p: HashSet<PeerId> = net_config
				.network_config
				.default_peers_set
				.reserved_nodes
				.iter()
				.map(|reserved| reserved.peer_id)
				.collect();
			no_slot_p.shrink_to_fit();
			no_slot_p
		};
		let default_peers_set_num_full =
			net_config.network_config.default_peers_set_num_full as usize;
		let default_peers_set_num_light = {
			let total = net_config.network_config.default_peers_set.out_peers +
				net_config.network_config.default_peers_set.in_peers;
			total.saturating_sub(net_config.network_config.default_peers_set_num_full) as usize
		};

		// Split warp sync params into warp sync config and a channel to retreive target block
		// header.
		let (warp_sync_config, warp_sync_target_block_header_rx) =
			warp_sync_params.map_or((None, None), |params| {
				let (config, target_block_rx) = params.split();
				(Some(config), target_block_rx)
			});

		// Make sure polling of the target block channel is a no-op if there is no block to
		// retrieve.
		let warp_sync_target_block_header_rx = warp_sync_target_block_header_rx
			.map_or(futures::future::pending().boxed().fuse(), |rx| rx.boxed().fuse());

		let (chain_sync, block_announce_config) = ChainSync::new(
			mode,
			client.clone(),
			protocol_id,
			fork_id,
			roles,
			max_parallel_downloads,
			max_blocks_per_request,
			warp_sync_config,
			metrics_registry,
			network_service.clone(),
			import_queue,
			block_request_protocol_name,
			state_request_protocol_name,
			warp_sync_protocol_name,
		)?;

		let block_announce_protocol_name = block_announce_config.notifications_protocol.clone();
		let (tx, service_rx) = tracing_unbounded("mpsc_chain_sync", 100_000);
		let num_connected = Arc::new(AtomicUsize::new(0));
		let is_major_syncing = Arc::new(AtomicBool::new(false));
		let genesis_hash = client
			.block_hash(0u32.into())
			.ok()
			.flatten()
			.expect("Genesis block exists; qed");

		// `default_peers_set.in_peers` contains an unspecified amount of light peers so the number
		// of full inbound peers must be calculated from the total full peer count
		let max_full_peers = net_config.network_config.default_peers_set_num_full;
		let max_out_peers = net_config.network_config.default_peers_set.out_peers;
		let max_in_peers = (max_full_peers - max_out_peers) as usize;

		Ok((
			Self {
				roles,
				client,
				chain_sync,
				network_service,
				peers: HashMap::new(),
				block_announce_data_cache: LruMap::new(ByLength::new(cache_capacity)),
				block_announce_protocol_name,
				block_announce_validator: BlockAnnounceValidatorStream::new(
					block_announce_validator,
				),
				num_connected: num_connected.clone(),
				is_major_syncing: is_major_syncing.clone(),
				service_rx,
				rx,
				genesis_hash,
				important_peers,
				default_peers_set_no_slot_connected_peers: HashSet::new(),
				warp_sync_target_block_header_rx,
				boot_node_ids,
				default_peers_set_no_slot_peers,
				default_peers_set_num_full,
				default_peers_set_num_light,
				num_in_peers: 0usize,
				max_in_peers,
				event_streams: Vec::new(),
				tick_timeout: Delay::new(TICK_TIMEOUT),
				syncing_started: None,
				last_notification_io: Instant::now(),
				metrics: if let Some(r) = metrics_registry {
					match Metrics::register(r, is_major_syncing.clone()) {
						Ok(metrics) => Some(metrics),
						Err(err) => {
							log::error!(target: LOG_TARGET, "Failed to register metrics {err:?}");
							None
						},
					}
				} else {
					None
				},
			},
			SyncingService::new(tx, num_connected, is_major_syncing),
			block_announce_config,
		))
	}

	/// Report Prometheus metrics.
	pub fn report_metrics(&self) {
		if let Some(metrics) = &self.metrics {
			let n = u64::try_from(self.peers.len()).unwrap_or(std::u64::MAX);
			metrics.peers.set(n);

			let m = self.chain_sync.metrics();

			metrics.fork_targets.set(m.fork_targets.into());
			metrics.queued_blocks.set(m.queued_blocks.into());

			metrics
				.justifications
				.with_label_values(&["pending"])
				.set(m.justifications.pending_requests.into());
			metrics
				.justifications
				.with_label_values(&["active"])
				.set(m.justifications.active_requests.into());
			metrics
				.justifications
				.with_label_values(&["failed"])
				.set(m.justifications.failed_requests.into());
			metrics
				.justifications
				.with_label_values(&["importing"])
				.set(m.justifications.importing_requests.into());
		}
	}

	fn update_peer_info(&mut self, peer_id: &PeerId) {
		if let Some(info) = self.chain_sync.peer_info(peer_id) {
			if let Some(ref mut peer) = self.peers.get_mut(peer_id) {
				peer.info.best_hash = info.best_hash;
				peer.info.best_number = info.best_number;
			}
		}
	}

	/// Process the result of the block announce validation.
	fn process_block_announce_validation_result(
		&mut self,
		validation_result: BlockAnnounceValidationResult<B::Header>,
	) {
		match validation_result {
			BlockAnnounceValidationResult::Skip { peer_id: _ } => {},
			BlockAnnounceValidationResult::Process { is_new_best, peer_id, announce } => {
				self.chain_sync.on_validated_block_announce(is_new_best, peer_id, &announce);

				self.update_peer_info(&peer_id);

				if let Some(data) = announce.data {
					if !data.is_empty() {
						self.block_announce_data_cache.insert(announce.header.hash(), data);
					}
				}
			},
			BlockAnnounceValidationResult::Failure { peer_id, disconnect } => {
				if disconnect {
					self.network_service
						.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
				}

				self.network_service.report_peer(peer_id, rep::BAD_BLOCK_ANNOUNCEMENT);
			},
		}
	}

	/// Push a block announce validation.
	pub fn push_block_announce_validation(
		&mut self,
		peer_id: PeerId,
		announce: BlockAnnounce<B::Header>,
	) {
		let hash = announce.header.hash();

		let peer = match self.peers.get_mut(&peer_id) {
			Some(p) => p,
			None => {
				log::error!(
					target: LOG_TARGET,
					"Received block announce from disconnected peer {peer_id}",
				);
				debug_assert!(false);
				return
			},
		};
		peer.known_blocks.insert(hash);

		if peer.info.roles.is_full() {
			let is_best = match announce.state.unwrap_or(BlockState::Best) {
				BlockState::Best => true,
				BlockState::Normal => false,
			};

			self.block_announce_validator
				.push_block_announce_validation(peer_id, hash, announce, is_best);
		}
	}

	/// Make sure an important block is propagated to peers.
	///
	/// In chain-based consensus, we often need to make sure non-best forks are
	/// at least temporarily synced.
	pub fn announce_block(&mut self, hash: B::Hash, data: Option<Vec<u8>>) {
		let header = match self.client.header(hash) {
			Ok(Some(header)) => header,
			Ok(None) => {
				log::warn!(target: LOG_TARGET, "Trying to announce unknown block: {hash}");
				return
			},
			Err(e) => {
				log::warn!(target: LOG_TARGET, "Error reading block header {hash}: {e}");
				return
			},
		};

		// don't announce genesis block since it will be ignored
		if header.number().is_zero() {
			return
		}

		let is_best = self.client.info().best_hash == hash;
		log::debug!(target: LOG_TARGET, "Reannouncing block {hash:?} is_best: {is_best}");

		let data = data
			.or_else(|| self.block_announce_data_cache.get(&hash).cloned())
			.unwrap_or_default();

		for (peer_id, ref mut peer) in self.peers.iter_mut() {
			let inserted = peer.known_blocks.insert(hash);
			if inserted {
				log::trace!(target: LOG_TARGET, "Announcing block {hash:?} to {peer_id}");
				let message = BlockAnnounce {
					header: header.clone(),
					state: if is_best { Some(BlockState::Best) } else { Some(BlockState::Normal) },
					data: Some(data.clone()),
				};

				self.last_notification_io = Instant::now();
				peer.sink.send_sync_notification(message.encode());
			}
		}
	}

	/// Inform sync about new best imported block.
	pub fn new_best_block_imported(&mut self, hash: B::Hash, number: NumberFor<B>) {
		log::debug!(target: LOG_TARGET, "New best block imported {hash:?}/#{number}");

		self.chain_sync.update_chain_info(&hash, number);
		self.network_service.set_notification_handshake(
			self.block_announce_protocol_name.clone(),
			BlockAnnouncesHandshake::<B>::build(self.roles, number, hash, self.genesis_hash)
				.encode(),
		)
	}

	pub async fn run(mut self) {

		log::info!(
			target: LOG_TARGET,
			"HALTING SYNC ENGINE"
		);

		futures::future::pending().await;

		log::info!(
			target: LOG_TARGET,
			"HOW DID YOU GET HERE??"
		);

		self.syncing_started = Some(Instant::now());

		loop {
			futures::future::poll_fn(|cx| self.poll(cx)).await;
		}
	}

	pub fn poll(&mut self, cx: &mut std::task::Context) -> Poll<()> {
		self.num_connected.store(self.peers.len(), Ordering::Relaxed);
		self.is_major_syncing
			.store(self.chain_sync.status().state.is_major_syncing(), Ordering::Relaxed);

		while let Poll::Ready(()) = self.tick_timeout.poll_unpin(cx) {
			self.report_metrics();
			self.tick_timeout.reset(TICK_TIMEOUT);

			// if `SyncingEngine` has just started, don't evict seemingly inactive peers right away
			// as they may not have produced blocks not because they've disconnected but because
			// they're still waiting to receive enough relaychain blocks to start producing blocks.
			if let Some(started) = self.syncing_started {
				if started.elapsed() < INITIAL_EVICTION_WAIT_PERIOD {
					continue
				}

				self.syncing_started = None;
				self.last_notification_io = Instant::now();
			}

			// if syncing hasn't sent or received any blocks within `INACTIVITY_EVICT_THRESHOLD`,
			// it means the local node has stalled and is connected to peers who either don't
			// consider it connected or are also all stalled. In order to unstall the node,
			// disconnect all peers and allow `ProtocolController` to establish new connections.
			if self.last_notification_io.elapsed() > INACTIVITY_EVICT_THRESHOLD {
				log::debug!(
					target: LOG_TARGET,
					"syncing has halted due to inactivity, evicting all peers",
				);

				for peer in self.peers.keys() {
					self.network_service.report_peer(*peer, rep::INACTIVE_SUBSTREAM);
					self.network_service
						.disconnect_peer(*peer, self.block_announce_protocol_name.clone());
				}

				// after all the peers have been evicted, start timer again to prevent evicting
				// new peers that join after the old peer have been evicted
				self.last_notification_io = Instant::now();
			}
		}

		while let Poll::Ready(Some(event)) = self.service_rx.poll_next_unpin(cx) {
			match event {
				ToServiceCommand::SetSyncForkRequest(peers, hash, number) => {
					self.chain_sync.set_sync_fork_request(peers, &hash, number);
				},
				ToServiceCommand::EventStream(tx) => self.event_streams.push(tx),
				ToServiceCommand::RequestJustification(hash, number) =>
					self.chain_sync.request_justification(&hash, number),
				ToServiceCommand::ClearJustificationRequests =>
					self.chain_sync.clear_justification_requests(),
				ToServiceCommand::BlocksProcessed(imported, count, results) => {
					for result in self.chain_sync.on_blocks_processed(imported, count, results) {
						match result {
							Ok((id, req)) => self.chain_sync.send_block_request(id, req),
							Err(BadPeer(id, repu)) => {
								self.network_service
									.disconnect_peer(id, self.block_announce_protocol_name.clone());
								self.network_service.report_peer(id, repu)
							},
						}
					}
				},
				ToServiceCommand::JustificationImported(peer_id, hash, number, success) => {
					self.chain_sync.on_justification_import(hash, number, success);
					if !success {
						log::info!(
							target: LOG_TARGET,
							"💔 Invalid justification provided by {peer_id} for #{hash}",
						);
						self.network_service
							.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
						self.network_service.report_peer(
							peer_id,
							ReputationChange::new_fatal("Invalid justification"),
						);
					}
				},
				ToServiceCommand::AnnounceBlock(hash, data) => self.announce_block(hash, data),
				ToServiceCommand::NewBestBlockImported(hash, number) =>
					self.new_best_block_imported(hash, number),
				ToServiceCommand::Status(tx) => {
					let mut status = self.chain_sync.status();
					status.num_connected_peers = self.peers.len() as u32;
					let _ = tx.send(status);
				},
				ToServiceCommand::NumActivePeers(tx) => {
					let _ = tx.send(self.chain_sync.num_active_peers());
				},
				ToServiceCommand::SyncState(tx) => {
					let _ = tx.send(self.chain_sync.status());
				},
				ToServiceCommand::BestSeenBlock(tx) => {
					let _ = tx.send(self.chain_sync.status().best_seen_block);
				},
				ToServiceCommand::NumSyncPeers(tx) => {
					let _ = tx.send(self.chain_sync.status().num_peers);
				},
				ToServiceCommand::NumQueuedBlocks(tx) => {
					let _ = tx.send(self.chain_sync.status().queued_blocks);
				},
				ToServiceCommand::NumDownloadedBlocks(tx) => {
					let _ = tx.send(self.chain_sync.num_downloaded_blocks());
				},
				ToServiceCommand::NumSyncRequests(tx) => {
					let _ = tx.send(self.chain_sync.num_sync_requests());
				},
				ToServiceCommand::PeersInfo(tx) => {
					let peers_info = self
						.peers
						.iter()
						.map(|(peer_id, peer)| (*peer_id, peer.info.clone()))
						.collect();
					let _ = tx.send(peers_info);
				},
				ToServiceCommand::OnBlockFinalized(hash, header) =>
					self.chain_sync.on_block_finalized(&hash, *header.number()),
			}
		}

		while let Poll::Ready(Some(event)) = self.rx.poll_next_unpin(cx) {
			match event {
				sc_network::SyncEvent::NotificationStreamOpened {
					remote,
					received_handshake,
					sink,
					inbound,
					tx,
				} => match self.on_sync_peer_connected(remote, &received_handshake, sink, inbound) {
					Ok(()) => {
						let _ = tx.send(true);
					},
					Err(()) => {
						log::debug!(
							target: LOG_TARGET,
							"Failed to register peer {remote:?}: {received_handshake:?}",
						);
						let _ = tx.send(false);
					},
				},
				sc_network::SyncEvent::NotificationStreamClosed { remote } => {
					if self.on_sync_peer_disconnected(remote).is_err() {
						log::trace!(
							target: LOG_TARGET,
							"Disconnected peer which had earlier been refused by on_sync_peer_connected {}",
							remote
						);
					}
				},
				sc_network::SyncEvent::NotificationsReceived { remote, messages } => {
					for message in messages {
						if self.peers.contains_key(&remote) {
							if let Ok(announce) = BlockAnnounce::decode(&mut message.as_ref()) {
								self.last_notification_io = Instant::now();
								self.push_block_announce_validation(remote, announce);
							} else {
								log::warn!(target: "sub-libp2p", "Failed to decode block announce");
							}
						} else {
							log::trace!(
								target: LOG_TARGET,
								"Received sync for peer earlier refused by sync layer: {remote}",
							);
						}
					}
				},
				sc_network::SyncEvent::NotificationSinkReplaced { remote, sink } => {
					if let Some(peer) = self.peers.get_mut(&remote) {
						peer.sink = sink;
					}
				},
			}
		}

		// Retreive warp sync target block header just before polling `ChainSync`
		// to make progress as soon as we receive it.
		match self.warp_sync_target_block_header_rx.poll_unpin(cx) {
			Poll::Ready(Ok(target)) => {
				self.chain_sync.set_warp_sync_target_block(target);
			},
			Poll::Ready(Err(err)) => {
				log::error!(
					target: LOG_TARGET,
					"Failed to get target block for warp sync. Error: {err:?}",
				);
			},
			Poll::Pending => {},
		}

		// Drive `ChainSync`.
		while let Poll::Ready(()) = self.chain_sync.poll(cx) {}

		// Poll block announce validations last, because if a block announcement was received
		// through the event stream between `SyncingEngine` and `Protocol` and the validation
		// finished right after it is queued, the resulting block request (if any) can be sent
		// right away.
		while let Poll::Ready(Some(result)) = self.block_announce_validator.poll_next_unpin(cx) {
			self.process_block_announce_validation_result(result);
		}

		Poll::Pending
	}

	/// Called by peer when it is disconnecting.
	///
	/// Returns a result if the handshake of this peer was indeed accepted.
	pub fn on_sync_peer_disconnected(&mut self, peer_id: PeerId) -> Result<(), ()> {
		if let Some(info) = self.peers.remove(&peer_id) {
			if self.important_peers.contains(&peer_id) {
				log::warn!(target: LOG_TARGET, "Reserved peer {peer_id} disconnected");
			} else {
				log::debug!(target: LOG_TARGET, "{peer_id} disconnected");
			}

			if !self.default_peers_set_no_slot_connected_peers.remove(&peer_id) &&
				info.inbound && info.info.roles.is_full()
			{
				match self.num_in_peers.checked_sub(1) {
					Some(value) => {
						self.num_in_peers = value;
					},
					None => {
						log::error!(
							target: LOG_TARGET,
							"trying to disconnect an inbound node which is not counted as inbound"
						);
						debug_assert!(false);
					},
				}
			}

			self.chain_sync.peer_disconnected(&peer_id);
			self.event_streams.retain(|stream| {
				stream.unbounded_send(SyncEvent::PeerDisconnected(peer_id)).is_ok()
			});
			Ok(())
		} else {
			Err(())
		}
	}

	/// Called on the first connection between two peers on the default set, after their exchange
	/// of handshake.
	///
	/// Returns `Ok` if the handshake is accepted and the peer added to the list of peers we sync
	/// from.
	pub fn on_sync_peer_connected(
		&mut self,
		peer_id: PeerId,
		status: &BlockAnnouncesHandshake<B>,
		sink: NotificationsSink,
		inbound: bool,
	) -> Result<(), ()> {
		log::trace!(target: LOG_TARGET, "New peer {peer_id} {status:?}");

		if self.peers.contains_key(&peer_id) {
			log::error!(
				target: LOG_TARGET,
				"Called on_sync_peer_connected with already connected peer {peer_id}",
			);
			debug_assert!(false);
			return Err(())
		}

		if status.genesis_hash != self.genesis_hash {
			self.network_service.report_peer(peer_id, rep::GENESIS_MISMATCH);

			if self.important_peers.contains(&peer_id) {
				log::error!(
					target: LOG_TARGET,
					"Reserved peer id `{}` is on a different chain (our genesis: {} theirs: {})",
					peer_id,
					self.genesis_hash,
					status.genesis_hash,
				);
			} else if self.boot_node_ids.contains(&peer_id) {
				log::error!(
					target: LOG_TARGET,
					"Bootnode with peer id `{}` is on a different chain (our genesis: {} theirs: {})",
					peer_id,
					self.genesis_hash,
					status.genesis_hash,
				);
			} else {
				log::debug!(
					target: LOG_TARGET,
					"Peer is on different chain (our genesis: {} theirs: {})",
					self.genesis_hash, status.genesis_hash
				);
			}

			return Err(())
		}

		let no_slot_peer = self.default_peers_set_no_slot_peers.contains(&peer_id);
		let this_peer_reserved_slot: usize = if no_slot_peer { 1 } else { 0 };

		// make sure to accept no more than `--in-peers` many full nodes
		if !no_slot_peer &&
			status.roles.is_full() &&
			inbound && self.num_in_peers == self.max_in_peers
		{
			log::debug!(
				target: LOG_TARGET,
				"All inbound slots have been consumed, rejecting {peer_id}",
			);
			return Err(())
		}

		if status.roles.is_full() &&
			self.chain_sync.num_peers() >=
				self.default_peers_set_num_full +
					self.default_peers_set_no_slot_connected_peers.len() +
					this_peer_reserved_slot
		{
			log::debug!(target: LOG_TARGET, "Too many full nodes, rejecting {peer_id}");
			return Err(())
		}

		if status.roles.is_light() &&
			(self.peers.len() - self.chain_sync.num_peers()) >= self.default_peers_set_num_light
		{
			// Make sure that not all slots are occupied by light clients.
			log::debug!(target: LOG_TARGET, "Too many light nodes, rejecting {peer_id}");
			return Err(())
		}

		let peer = Peer {
			info: ExtendedPeerInfo {
				roles: status.roles,
				best_hash: status.best_hash,
				best_number: status.best_number,
			},
			known_blocks: LruHashSet::new(
				NonZeroUsize::new(MAX_KNOWN_BLOCKS).expect("Constant is nonzero"),
			),
			sink,
			inbound,
		};

		let req = if peer.info.roles.is_full() {
			match self.chain_sync.new_peer(peer_id, peer.info.best_hash, peer.info.best_number) {
				Ok(req) => req,
				Err(BadPeer(id, repu)) => {
					self.network_service.report_peer(id, repu);
					return Err(())
				},
			}
		} else {
			None
		};

		log::debug!(target: LOG_TARGET, "Connected {peer_id}");

		self.peers.insert(peer_id, peer);

		if no_slot_peer {
			self.default_peers_set_no_slot_connected_peers.insert(peer_id);
		} else if inbound && status.roles.is_full() {
			self.num_in_peers += 1;
		}

		if let Some(req) = req {
			self.chain_sync.send_block_request(peer_id, req);
		}

		self.event_streams
			.retain(|stream| stream.unbounded_send(SyncEvent::PeerConnected(peer_id)).is_ok());

		Ok(())
	}
}
