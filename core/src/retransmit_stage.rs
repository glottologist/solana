//! The `retransmit_stage` retransmits shreds between validators
#![allow(clippy::rc_buffer)]

use {
    crate::{
        ancestor_hashes_service::AncestorHashesReplayUpdateReceiver,
        cluster_info_vote_listener::VerifiedVoteReceiver,
        cluster_nodes::ClusterNodesCache,
        cluster_slots::ClusterSlots,
        cluster_slots_service::{ClusterSlotsService, ClusterSlotsUpdateReceiver},
        completed_data_sets_service::CompletedDataSetsSender,
        packet_hasher::PacketHasher,
        repair_service::{DuplicateSlotsResetSender, RepairInfo},
        result::{Error, Result},
        window_service::{should_retransmit_and_persist, WindowService},
    },
    crossbeam_channel::{Receiver, Sender},
    lru::LruCache,
    solana_client::rpc_response::SlotUpdate,
    solana_gossip::cluster_info::{ClusterInfo, DATA_PLANE_FANOUT},
    solana_ledger::{
        shred::Shred,
        {blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    },
    solana_measure::measure::Measure,
    solana_metrics::inc_new_counter_error,
    solana_perf::packet::Packets,
    solana_rpc::{max_slots::MaxSlots, rpc_subscriptions::RpcSubscriptions},
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_sdk::{
        clock::Slot,
        epoch_schedule::EpochSchedule,
        pubkey::Pubkey,
        timing::{timestamp, AtomicInterval},
    },
    std::{
        collections::{BTreeSet, HashSet},
        net::UdpSocket,
        ops::DerefMut,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc::{self, channel, RecvTimeoutError},
            Arc, Mutex, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

const MAX_DUPLICATE_COUNT: usize = 2;
const DEFAULT_LRU_SIZE: usize = 10_000;

// Limit a given thread to consume about this many shreds so that
// it doesn't pull up too much work.
const MAX_SHREDS_BATCH_SIZE: usize = 100;

const CLUSTER_NODES_CACHE_NUM_EPOCH_CAP: usize = 8;
const CLUSTER_NODES_CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct RetransmitStats {
    num_shreds: AtomicU64,
    num_shreds_skipped: AtomicU64,
    total_batches: AtomicU64,
    total_time: AtomicU64,
    epoch_fetch: AtomicU64,
    epoch_cache_update: AtomicU64,
    retransmit_total: AtomicU64,
    last_ts: AtomicInterval,
    compute_turbine_peers_total: AtomicU64,
}

#[allow(clippy::too_many_arguments)]
fn update_retransmit_stats(
    stats: &RetransmitStats,
    total_time: u64,
    num_shreds: usize,
    num_shreds_skipped: usize,
    retransmit_total: u64,
    compute_turbine_peers_total: u64,
    peers_len: usize,
    epoch_fetch: u64,
    epoch_cach_update: u64,
) {
    stats.total_time.fetch_add(total_time, Ordering::Relaxed);
    stats
        .num_shreds
        .fetch_add(num_shreds as u64, Ordering::Relaxed);
    stats
        .num_shreds_skipped
        .fetch_add(num_shreds_skipped as u64, Ordering::Relaxed);
    stats
        .retransmit_total
        .fetch_add(retransmit_total, Ordering::Relaxed);
    stats
        .compute_turbine_peers_total
        .fetch_add(compute_turbine_peers_total, Ordering::Relaxed);
    stats.total_batches.fetch_add(1, Ordering::Relaxed);
    stats.epoch_fetch.fetch_add(epoch_fetch, Ordering::Relaxed);
    stats
        .epoch_cache_update
        .fetch_add(epoch_cach_update, Ordering::Relaxed);
    if stats.last_ts.should_update(2000) {
        datapoint_info!("retransmit-num_nodes", ("count", peers_len, i64));
        datapoint_info!(
            "retransmit-stage",
            (
                "total_time",
                stats.total_time.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "epoch_fetch",
                stats.epoch_fetch.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "epoch_cache_update",
                stats.epoch_cache_update.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "total_batches",
                stats.total_batches.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "num_shreds",
                stats.num_shreds.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "num_shreds_skipped",
                stats.num_shreds_skipped.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "retransmit_total",
                stats.retransmit_total.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "compute_turbine",
                stats.compute_turbine_peers_total.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
        );
    }
}

// Map of shred (slot, index, is_data) => list of hash values seen for that key.
type ShredFilter = LruCache<(Slot, u32, bool), Vec<u64>>;

type ShredFilterAndHasher = (ShredFilter, PacketHasher);

// Returns true if shred is already received and should skip retransmit.
fn should_skip_retransmit(shred: &Shred, shreds_received: &Mutex<ShredFilterAndHasher>) -> bool {
    let key = (shred.slot(), shred.index(), shred.is_data());
    let mut shreds_received = shreds_received.lock().unwrap();
    let (cache, hasher) = shreds_received.deref_mut();
    match cache.get_mut(&key) {
        Some(sent) if sent.len() >= MAX_DUPLICATE_COUNT => true,
        Some(sent) => {
            let hash = hasher.hash_shred(shred);
            if sent.contains(&hash) {
                true
            } else {
                sent.push(hash);
                false
            }
        }
        None => {
            let hash = hasher.hash_shred(shred);
            cache.put(key, vec![hash]);
            false
        }
    }
}

// Returns true if this is the first time receiving a shred for `shred_slot`.
fn check_if_first_shred_received(
    shred_slot: Slot,
    first_shreds_received: &Mutex<BTreeSet<Slot>>,
    root_bank: &Bank,
) -> bool {
    if shred_slot <= root_bank.slot() {
        return false;
    }

    let mut first_shreds_received_locked = first_shreds_received.lock().unwrap();
    if !first_shreds_received_locked.contains(&shred_slot) {
        datapoint_info!("retransmit-first-shred", ("slot", shred_slot, i64));
        first_shreds_received_locked.insert(shred_slot);
        if first_shreds_received_locked.len() > 100 {
            let mut slots_before_root =
                first_shreds_received_locked.split_off(&(root_bank.slot() + 1));
            // `slots_before_root` now contains all slots <= root
            std::mem::swap(&mut slots_before_root, &mut first_shreds_received_locked);
        }
        true
    } else {
        false
    }
}

fn maybe_reset_shreds_received_cache(
    shreds_received: &Mutex<ShredFilterAndHasher>,
    hasher_reset_ts: &AtomicU64,
) {
    const UPDATE_INTERVAL_MS: u64 = 1000;
    let now = timestamp();
    let prev = hasher_reset_ts.load(Ordering::Acquire);
    if now.saturating_sub(prev) > UPDATE_INTERVAL_MS
        && hasher_reset_ts
            .compare_exchange(prev, now, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        let mut shreds_received = shreds_received.lock().unwrap();
        let (cache, hasher) = shreds_received.deref_mut();
        cache.clear();
        hasher.reset();
    }
}

#[allow(clippy::too_many_arguments)]
fn retransmit(
    bank_forks: &RwLock<BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_info: &ClusterInfo,
    shreds_receiver: &Mutex<mpsc::Receiver<Vec<Shred>>>,
    sock: &UdpSocket,
    id: u32,
    stats: &RetransmitStats,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    hasher_reset_ts: &AtomicU64,
    shreds_received: &Mutex<ShredFilterAndHasher>,
    max_slots: &MaxSlots,
    first_shreds_received: &Mutex<BTreeSet<Slot>>,
    rpc_subscriptions: Option<&RpcSubscriptions>,
) -> Result<()> {
    const RECV_TIMEOUT: Duration = Duration::from_secs(1);
    let shreds_receiver = shreds_receiver.lock().unwrap();
    let mut shreds = shreds_receiver.recv_timeout(RECV_TIMEOUT)?;
    let mut timer_start = Measure::start("retransmit");
    while let Ok(more_shreds) = shreds_receiver.try_recv() {
        shreds.extend(more_shreds);
        if shreds.len() >= MAX_SHREDS_BATCH_SIZE {
            break;
        }
    }
    drop(shreds_receiver);

    let mut epoch_fetch = Measure::start("retransmit_epoch_fetch");
    let (working_bank, root_bank) = {
        let bank_forks = bank_forks.read().unwrap();
        (bank_forks.working_bank(), bank_forks.root_bank())
    };
    epoch_fetch.stop();

    let mut epoch_cache_update = Measure::start("retransmit_epoch_cach_update");
    maybe_reset_shreds_received_cache(shreds_received, hasher_reset_ts);
    epoch_cache_update.stop();

    let num_shreds = shreds.len();
    let my_id = cluster_info.id();
    let socket_addr_space = cluster_info.socket_addr_space();
    let mut retransmit_total = 0;
    let mut num_shreds_skipped = 0;
    let mut compute_turbine_peers_total = 0;
    let mut max_slot = 0;
    for shred in shreds {
        if should_skip_retransmit(&shred, shreds_received) {
            num_shreds_skipped += 1;
            continue;
        }
        let shred_slot = shred.slot();
        max_slot = max_slot.max(shred_slot);

        if let Some(rpc_subscriptions) = rpc_subscriptions {
            if check_if_first_shred_received(shred_slot, first_shreds_received, &root_bank) {
                rpc_subscriptions.notify_slot_update(SlotUpdate::FirstShredReceived {
                    slot: shred_slot,
                    timestamp: timestamp(),
                });
            }
        }

        let mut compute_turbine_peers = Measure::start("turbine_start");
        // TODO: consider using root-bank here for leader lookup!
        // Shreds' signatures should be verified before they reach here, and if
        // the leader is unknown they should fail signature check. So here we
        // should expect to know the slot leader and otherwise skip the shred.
        let slot_leader =
            match leader_schedule_cache.slot_leader_at(shred_slot, Some(&working_bank)) {
                Some(pubkey) => pubkey,
                None => continue,
            };
        let cluster_nodes =
            cluster_nodes_cache.get(shred_slot, &root_bank, &working_bank, cluster_info);
        let shred_seed = shred.seed(slot_leader, &root_bank);
        let (neighbors, children) =
            cluster_nodes.get_retransmit_peers(shred_seed, DATA_PLANE_FANOUT, slot_leader);
        let anchor_node = neighbors[0].id == my_id;
        compute_turbine_peers.stop();
        compute_turbine_peers_total += compute_turbine_peers.as_us();

        let mut retransmit_time = Measure::start("retransmit_to");
        // If the node is on the critical path (i.e. the first node in each
        // neighborhood), it should send the packet to tvu socket of its
        // children and also tvu_forward socket of its neighbors. Otherwise it
        // should only forward to tvu_forward socket of its children.
        if anchor_node {
            // First neighbor is this node itself, so skip it.
            ClusterInfo::retransmit_to(
                &neighbors[1..],
                &shred.payload,
                sock,
                true, // forward socket
                socket_addr_space,
            );
        }
        ClusterInfo::retransmit_to(
            &children,
            &shred.payload,
            sock,
            !anchor_node, // send to forward socket!
            socket_addr_space,
        );
        retransmit_time.stop();
        retransmit_total += retransmit_time.as_us();
    }
    max_slots.retransmit.fetch_max(max_slot, Ordering::Relaxed);
    timer_start.stop();
    debug!(
        "retransmitted {} shreds in {}ms retransmit_time: {}ms id: {}",
        num_shreds,
        timer_start.as_ms(),
        retransmit_total,
        id,
    );
    let cluster_nodes =
        cluster_nodes_cache.get(root_bank.slot(), &root_bank, &working_bank, cluster_info);
    update_retransmit_stats(
        stats,
        timer_start.as_us(),
        num_shreds,
        num_shreds_skipped,
        retransmit_total,
        compute_turbine_peers_total,
        cluster_nodes.num_peers(),
        epoch_fetch.as_us(),
        epoch_cache_update.as_us(),
    );

    Ok(())
}

/// Service to retransmit messages from the leader or layer 1 to relevant peer nodes.
/// See `cluster_info` for network layer definitions.
/// # Arguments
/// * `sockets` - Sockets to read from.
/// * `bank_forks` - The BankForks structure
/// * `leader_schedule_cache` - The leader schedule to verify shreds
/// * `cluster_info` - This structure needs to be updated and populated by the bank and via gossip.
/// * `r` - Receive channel for shreds to be retransmitted to all the layer 1 nodes.
pub fn retransmitter(
    sockets: Arc<Vec<UdpSocket>>,
    bank_forks: Arc<RwLock<BankForks>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    cluster_info: Arc<ClusterInfo>,
    shreds_receiver: Arc<Mutex<mpsc::Receiver<Vec<Shred>>>>,
    max_slots: Arc<MaxSlots>,
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
) -> Vec<JoinHandle<()>> {
    let cluster_nodes_cache = Arc::new(ClusterNodesCache::<RetransmitStage>::new(
        CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
        CLUSTER_NODES_CACHE_TTL,
    ));
    let hasher_reset_ts = Arc::default();
    let stats = Arc::new(RetransmitStats::default());
    let shreds_received = Arc::new(Mutex::new((
        LruCache::new(DEFAULT_LRU_SIZE),
        PacketHasher::default(),
    )));
    let first_shreds_received = Arc::new(Mutex::new(BTreeSet::new()));
    (0..sockets.len())
        .map(|s| {
            let sockets = sockets.clone();
            let bank_forks = bank_forks.clone();
            let leader_schedule_cache = leader_schedule_cache.clone();
            let shreds_receiver = shreds_receiver.clone();
            let cluster_info = cluster_info.clone();
            let stats = stats.clone();
            let cluster_nodes_cache = Arc::clone(&cluster_nodes_cache);
            let hasher_reset_ts = Arc::clone(&hasher_reset_ts);
            let shreds_received = shreds_received.clone();
            let max_slots = max_slots.clone();
            let first_shreds_received = first_shreds_received.clone();
            let rpc_subscriptions = rpc_subscriptions.clone();

            Builder::new()
                .name("solana-retransmitter".to_string())
                .spawn(move || {
                    trace!("retransmitter started");
                    loop {
                        if let Err(e) = retransmit(
                            &bank_forks,
                            &leader_schedule_cache,
                            &cluster_info,
                            &shreds_receiver,
                            &sockets[s],
                            s as u32,
                            &stats,
                            &cluster_nodes_cache,
                            &hasher_reset_ts,
                            &shreds_received,
                            &max_slots,
                            &first_shreds_received,
                            rpc_subscriptions.as_deref(),
                        ) {
                            match e {
                                Error::RecvTimeout(RecvTimeoutError::Disconnected) => break,
                                Error::RecvTimeout(RecvTimeoutError::Timeout) => (),
                                _ => {
                                    inc_new_counter_error!("streamer-retransmit-error", 1, 1);
                                }
                            }
                        }
                    }
                    trace!("exiting retransmitter");
                })
                .unwrap()
        })
        .collect()
}

pub(crate) struct RetransmitStage {
    thread_hdls: Vec<JoinHandle<()>>,
    window_service: WindowService,
    cluster_slots_service: ClusterSlotsService,
}

impl RetransmitStage {
    #[allow(clippy::new_ret_no_self)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        blockstore: Arc<Blockstore>,
        cluster_info: Arc<ClusterInfo>,
        retransmit_sockets: Arc<Vec<UdpSocket>>,
        repair_socket: Arc<UdpSocket>,
        verified_receiver: Receiver<Vec<Packets>>,
        exit: Arc<AtomicBool>,
        cluster_slots_update_receiver: ClusterSlotsUpdateReceiver,
        epoch_schedule: EpochSchedule,
        cfg: Option<Arc<AtomicBool>>,
        shred_version: u16,
        cluster_slots: Arc<ClusterSlots>,
        duplicate_slots_reset_sender: DuplicateSlotsResetSender,
        verified_vote_receiver: VerifiedVoteReceiver,
        repair_validators: Option<HashSet<Pubkey>>,
        completed_data_sets_sender: CompletedDataSetsSender,
        max_slots: Arc<MaxSlots>,
        rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
        duplicate_slots_sender: Sender<Slot>,
        ancestor_hashes_replay_update_receiver: AncestorHashesReplayUpdateReceiver,
    ) -> Self {
        let (retransmit_sender, retransmit_receiver) = channel();
        // https://github.com/rust-lang/rust/issues/39364#issuecomment-634545136
        let _retransmit_sender = retransmit_sender.clone();

        let retransmit_receiver = Arc::new(Mutex::new(retransmit_receiver));
        let thread_hdls = retransmitter(
            retransmit_sockets,
            bank_forks.clone(),
            leader_schedule_cache.clone(),
            cluster_info.clone(),
            retransmit_receiver,
            max_slots,
            rpc_subscriptions,
        );

        let cluster_slots_service = ClusterSlotsService::new(
            blockstore.clone(),
            cluster_slots.clone(),
            bank_forks.clone(),
            cluster_info.clone(),
            cluster_slots_update_receiver,
            exit.clone(),
        );

        let leader_schedule_cache_clone = leader_schedule_cache.clone();
        let repair_info = RepairInfo {
            bank_forks,
            epoch_schedule,
            duplicate_slots_reset_sender,
            repair_validators,
            cluster_info,
            cluster_slots,
        };
        let window_service = WindowService::new(
            blockstore,
            verified_receiver,
            retransmit_sender,
            repair_socket,
            exit,
            repair_info,
            leader_schedule_cache,
            move |id, shred, working_bank, last_root| {
                let is_connected = cfg
                    .as_ref()
                    .map(|x| x.load(Ordering::Relaxed))
                    .unwrap_or(true);
                let rv = should_retransmit_and_persist(
                    shred,
                    working_bank,
                    &leader_schedule_cache_clone,
                    id,
                    last_root,
                    shred_version,
                );
                rv && is_connected
            },
            verified_vote_receiver,
            completed_data_sets_sender,
            duplicate_slots_sender,
            ancestor_hashes_replay_update_receiver,
        );

        Self {
            thread_hdls,
            window_service,
            cluster_slots_service,
        }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        self.window_service.join()?;
        self.cluster_slots_service.join()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_gossip::contact_info::ContactInfo,
        solana_ledger::{
            blockstore_processor::{process_blockstore, ProcessOptions},
            create_new_tmp_ledger,
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
        },
        solana_net_utils::find_available_port_in_range,
        solana_sdk::signature::Keypair,
        solana_streamer::socket::SocketAddrSpace,
        std::net::{IpAddr, Ipv4Addr},
    };

    #[test]
    fn test_skip_repair() {
        solana_logger::setup();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(123);
        let (ledger_path, _blockhash) = create_new_tmp_ledger!(&genesis_config);
        let blockstore = Blockstore::open(&ledger_path).unwrap();
        let opts = ProcessOptions {
            accounts_db_test_hash_calculation: true,
            full_leader_cache: true,
            ..ProcessOptions::default()
        };
        let (bank_forks, cached_leader_schedule) =
            process_blockstore(&genesis_config, &blockstore, Vec::new(), opts, None).unwrap();
        let leader_schedule_cache = Arc::new(cached_leader_schedule);
        let bank_forks = Arc::new(RwLock::new(bank_forks));

        let mut me = ContactInfo::new_localhost(&solana_sdk::pubkey::new_rand(), 0);
        let ip_addr = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
        let port = find_available_port_in_range(ip_addr, (8000, 10000)).unwrap();
        let me_retransmit = UdpSocket::bind(format!("127.0.0.1:{}", port)).unwrap();
        // need to make sure tvu and tpu are valid addresses
        me.tvu_forwards = me_retransmit.local_addr().unwrap();

        let port = find_available_port_in_range(ip_addr, (8000, 10000)).unwrap();
        me.tvu = UdpSocket::bind(format!("127.0.0.1:{}", port))
            .unwrap()
            .local_addr()
            .unwrap();
        // This fixes the order of nodes returned by shuffle_peers_and_index,
        // and makes turbine retransmit tree deterministic for the purpose of
        // the test.
        let other = std::iter::repeat_with(solana_sdk::pubkey::new_rand)
            .find(|pk| me.id < *pk)
            .unwrap();
        let other = ContactInfo::new_localhost(&other, 0);
        let cluster_info = ClusterInfo::new(
            other,
            Arc::new(Keypair::new()),
            SocketAddrSpace::Unspecified,
        );
        cluster_info.insert_info(me);

        let retransmit_socket = Arc::new(vec![UdpSocket::bind("0.0.0.0:0").unwrap()]);
        let cluster_info = Arc::new(cluster_info);

        let (retransmit_sender, retransmit_receiver) = channel();
        let _retransmit_sender = retransmit_sender.clone();
        let _t_retransmit = retransmitter(
            retransmit_socket,
            bank_forks,
            leader_schedule_cache,
            cluster_info,
            Arc::new(Mutex::new(retransmit_receiver)),
            Arc::default(), // MaxSlots
            None,
        );

        let shred = Shred::new_from_data(0, 0, 0, None, true, true, 0, 0x20, 0);
        // it should send this over the sockets.
        retransmit_sender.send(vec![shred]).unwrap();
        let mut packets = Packets::new(vec![]);
        solana_streamer::packet::recv_from(&mut packets, &me_retransmit, 1).unwrap();
        assert_eq!(packets.packets.len(), 1);
        assert!(!packets.packets[0].meta.repair);
    }

    #[test]
    fn test_already_received() {
        let slot = 1;
        let index = 5;
        let version = 0x40;
        let shred = Shred::new_from_data(slot, index, 0, None, true, true, 0, version, 0);
        let shreds_received = Arc::new(Mutex::new((LruCache::new(100), PacketHasher::default())));
        // unique shred for (1, 5) should pass
        assert!(!should_skip_retransmit(&shred, &shreds_received));
        // duplicate shred for (1, 5) blocked
        assert!(should_skip_retransmit(&shred, &shreds_received));

        let shred = Shred::new_from_data(slot, index, 2, None, true, true, 0, version, 0);
        // first duplicate shred for (1, 5) passed
        assert!(!should_skip_retransmit(&shred, &shreds_received));
        // then blocked
        assert!(should_skip_retransmit(&shred, &shreds_received));

        let shred = Shred::new_from_data(slot, index, 8, None, true, true, 0, version, 0);
        // 2nd duplicate shred for (1, 5) blocked
        assert!(should_skip_retransmit(&shred, &shreds_received));
        assert!(should_skip_retransmit(&shred, &shreds_received));

        let shred = Shred::new_empty_coding(slot, index, 0, 1, 1, version);
        // Coding at (1, 5) passes
        assert!(!should_skip_retransmit(&shred, &shreds_received));
        // then blocked
        assert!(should_skip_retransmit(&shred, &shreds_received));

        let shred = Shred::new_empty_coding(slot, index, 2, 1, 1, version);
        // 2nd unique coding at (1, 5) passes
        assert!(!should_skip_retransmit(&shred, &shreds_received));
        // same again is blocked
        assert!(should_skip_retransmit(&shred, &shreds_received));

        let shred = Shred::new_empty_coding(slot, index, 3, 1, 1, version);
        // Another unique coding at (1, 5) always blocked
        assert!(should_skip_retransmit(&shred, &shreds_received));
        assert!(should_skip_retransmit(&shred, &shreds_received));
    }
}
