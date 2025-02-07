use async_channel::unbounded;
use clap::Parser;
use futures::{future::try_join_all, Future};
use itertools::Itertools;
use kaspa_consensus::{
    config::ConfigBuilder,
    consensus::Consensus,
    constants::perf::PerfParams,
    model::stores::{
        block_transactions::BlockTransactionsStoreReader,
        ghostdag::{GhostdagStoreReader, KType},
        headers::HeaderStoreReader,
        relations::RelationsStoreReader,
    },
    params::{Params, Testnet11Bps, DEVNET_PARAMS, TESTNET11_PARAMS},
};
use kaspa_consensus_core::{
    api::ConsensusApi, block::Block, blockstatus::BlockStatus, config::bps::calculate_ghostdag_k, errors::block::BlockProcessResult,
    BlockHashSet, BlockLevel, HashMapCustomHasher,
};
use kaspa_consensus_notify::root::ConsensusNotificationRoot;
use kaspa_core::{info, warn};
use kaspa_database::utils::{create_temp_db_with_parallelism, load_existing_db};
use kaspa_hashes::Hash;
use simulator::network::KaspaNetworkSimulator;
use std::{collections::VecDeque, sync::Arc};

pub mod simulator;

/// Kaspa Network Simulator
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Simulation blocks per second
    #[arg(short, long, default_value_t = 1.0)]
    bps: f64,

    /// Simulation delay (seconds)
    #[arg(short, long, default_value_t = 2.0)]
    delay: f64,

    /// Number of miners
    #[arg(short, long, default_value_t = 1)]
    miners: u64,

    /// Target transactions per block
    #[arg(short, long, default_value_t = 200)]
    tpb: u64,

    /// Target simulation time (seconds)
    #[arg(short, long, default_value_t = 600)]
    sim_time: u64,

    /// Target number of blocks the simulation should produce (overrides --sim-time if specified)
    #[arg(short = 'n', long)]
    target_blocks: Option<u64>,

    /// Number of pool-thread threads used by the header and body processors.
    /// Defaults to the number of logical CPU cores.
    #[arg(short, long)]
    processors_threads: Option<usize>,

    /// Number of pool-thread threads used by the virtual processor (for parallel transaction verification).
    /// Defaults to the number of logical CPU cores.
    #[arg(short, long)]
    virtual_threads: Option<usize>,

    /// Logging level for all subsystems {off, error, warn, info, debug, trace}
    ///  -- You may also specify <subsystem>=<level>,<subsystem2>=<level>,... to set the log level for individual subsystems
    #[arg(long = "loglevel", default_value = format!("info,{}=trace", env!("CARGO_PKG_NAME")))]
    log_level: String,

    /// Output directory to save the simulation DB
    #[arg(short, long)]
    output_dir: Option<String>,

    /// Input directory of a previous simulation DB (NOTE: simulation args must be compatible with the original run)
    #[arg(short, long)]
    input_dir: Option<String>,

    /// Indicates whether to test pruning. Currently this means we shorten the pruning constants and avoid validating
    /// the DAG in a separate consensus following the simulation phase
    #[arg(long, default_value_t = false)]
    test_pruning: bool,

    /// Use the legacy full-window DAA mechanism (note: the size of this window scales with bps)
    #[arg(long, default_value_t = false)]
    daa_legacy: bool,

    /// Use testnet-11 consensus params
    #[arg(long, default_value_t = false)]
    testnet11: bool,
}

fn main() {
    // Get CLI arguments
    let mut args = Args::parse();

    // Initialize the logger
    kaspa_core::log::init_logger(None, &args.log_level);

    // Print package name and version
    info!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

    // Configure the panic behavior
    kaspa_core::panic::configure_panic();

    assert!(args.bps * args.delay < 250.0, "The delay times bps product is larger than 250");
    if args.miners > 1 {
        warn!(
            "Warning: number of miners was configured to {}. Currently each miner added doubles the simulation 
        memory and runtime footprint, while a single miner is sufficient for most simulation purposes (delay is simulated anyway).",
            args.miners
        );
    }
    args.bps = if args.testnet11 { Testnet11Bps::bps() as f64 } else { args.bps };
    let params = if args.testnet11 { TESTNET11_PARAMS } else { DEVNET_PARAMS };
    let mut builder = ConfigBuilder::new(params)
        .apply_args(|config| apply_args_to_consensus_params(&args, &mut config.params))
        .apply_args(|config| apply_args_to_perf_params(&args, &mut config.perf))
        .adjust_perf_params_to_consensus_params()
        .skip_proof_of_work()
        .enable_sanity_checks();
    if !args.test_pruning {
        builder = builder.set_archival();
    }
    let config = Arc::new(builder.build());

    // Load an existing consensus or run the simulation
    let (consensus, _lifetime) = if let Some(input_dir) = args.input_dir {
        let (lifetime, db) = load_existing_db(input_dir, num_cpus::get());
        let (dummy_notification_sender, _) = unbounded();
        let notification_root = Arc::new(ConsensusNotificationRoot::new(dummy_notification_sender));
        let consensus = Arc::new(Consensus::new(db, config.clone(), Default::default(), notification_root, Default::default()));
        (consensus, lifetime)
    } else {
        let until = if args.target_blocks.is_none() { config.genesis.timestamp + args.sim_time * 1000 } else { u64::MAX }; // milliseconds
        let mut sim = KaspaNetworkSimulator::new(args.delay, args.bps, args.target_blocks, config.clone(), args.output_dir);
        let (consensus, handles, lifetime) = sim.init(args.miners, args.tpb).run(until);
        consensus.shutdown(handles);
        (consensus, lifetime)
    };

    if args.test_pruning {
        drop(consensus);
        return;
    }

    // Benchmark the DAG validation time
    let (_lifetime2, db2) = create_temp_db_with_parallelism(num_cpus::get());
    let (dummy_notification_sender, _) = unbounded();
    let notification_root = Arc::new(ConsensusNotificationRoot::new(dummy_notification_sender));
    let consensus2 = Arc::new(Consensus::new(db2, config.clone(), Default::default(), notification_root, Default::default()));
    let handles2 = consensus2.run_processors();
    validate(&consensus, &consensus2, &config, args.delay, args.bps);
    consensus2.shutdown(handles2);
    drop(consensus);
}

fn apply_args_to_consensus_params(args: &Args, params: &mut Params) {
    // We have no actual PoW in the simulation, so the true max is most reflective,
    // however we avoid the actual max since it is reserved for the DB prefix scheme
    params.max_block_level = BlockLevel::MAX - 1;
    params.genesis.timestamp = 0;
    if args.testnet11 {
        info!(
            "Using kaspa-testnet-11 configuration (GHOSTDAG K={}, DAA window size={}, Median time window size={})",
            params.ghostdag_k,
            params.difficulty_window_size(0),
            params.past_median_time_window_size(0),
        );
    } else if args.bps * args.delay > 2.0 {
        let k = u64::max(calculate_ghostdag_k(2.0 * args.delay * args.bps, 0.05), params.ghostdag_k as u64);
        let k = u64::min(k, KType::MAX as u64) as KType; // Clamp to KType::MAX
        params.ghostdag_k = k;
        params.mergeset_size_limit = k as u64 * 10;
        params.max_block_parents = u8::max((0.66 * k as f64) as u8, 10);
        params.target_time_per_block = (1000.0 / args.bps) as u64;
        params.merge_depth = (params.merge_depth as f64 * args.bps) as u64;
        params.coinbase_maturity = (params.coinbase_maturity as f64 * f64::max(1.0, args.bps * args.delay * 0.25)) as u64;

        if args.daa_legacy {
            // Scale DAA and median-time windows linearly with BPS
            params.sampling_activation_daa_score = u64::MAX;
            params.legacy_timestamp_deviation_tolerance = (params.legacy_timestamp_deviation_tolerance as f64 * args.bps) as u64;
            params.legacy_difficulty_window_size = (params.legacy_difficulty_window_size as f64 * args.bps) as usize;
        } else {
            // Use the new sampling algorithms
            params.sampling_activation_daa_score = 0;
            params.past_median_time_sample_rate = (10.0 * args.bps) as u64;
            params.new_timestamp_deviation_tolerance = (600.0 * args.bps) as u64;
            params.difficulty_sample_rate = (2.0 * args.bps) as u64;
        }

        info!(
            "The delay times bps product is larger than 2 (2Dλ={}), setting GHOSTDAG K={}, DAA window size={})",
            2.0 * args.delay * args.bps,
            k,
            params.difficulty_window_size(0)
        );
    }
    if args.test_pruning {
        params.pruning_proof_m = 16;
        params.legacy_difficulty_window_size = 64;
        params.legacy_timestamp_deviation_tolerance = 16;
        params.finality_depth = 128;
        params.merge_depth = 128;
        params.mergeset_size_limit = 32;
        params.pruning_depth = params.anticone_finalization_depth();
        info!("Setting pruning depth to {}", params.pruning_depth);
    }
}

fn apply_args_to_perf_params(args: &Args, perf_params: &mut PerfParams) {
    if let Some(processors_pool_threads) = args.processors_threads {
        perf_params.block_processors_num_threads = processors_pool_threads;
    }
    if let Some(virtual_pool_threads) = args.virtual_threads {
        perf_params.virtual_processor_num_threads = virtual_pool_threads;
    }
}

#[tokio::main]
async fn validate(src_consensus: &Consensus, dst_consensus: &Consensus, params: &Params, delay: f64, bps: f64) {
    let hashes = topologically_ordered_hashes(src_consensus, params.genesis.hash);
    let num_blocks = hashes.len();
    let num_txs = print_stats(src_consensus, &hashes, delay, bps, params.ghostdag_k);
    info!("Validating {num_blocks} blocks with {num_txs} transactions overall...");
    let start = std::time::Instant::now();
    let chunks = hashes.into_iter().chunks(1000);
    let mut iter = chunks.into_iter();
    let mut chunk = iter.next().unwrap();
    let mut prev_joins = submit_chunk(src_consensus, dst_consensus, &mut chunk);

    for mut chunk in iter {
        let current_joins = submit_chunk(src_consensus, dst_consensus, &mut chunk);
        let statuses = try_join_all(prev_joins).await.unwrap();
        assert!(statuses.iter().all(|s| s.is_utxo_valid_or_pending()));
        prev_joins = current_joins;
    }

    let statuses = try_join_all(prev_joins).await.unwrap();
    assert!(statuses.iter().all(|s| s.is_utxo_valid_or_pending()));

    // Assert that at least one body tip was resolved with valid UTXO
    assert!(dst_consensus.body_tips().iter().copied().any(|h| dst_consensus.block_status(h) == BlockStatus::StatusUTXOValid));
    let elapsed = start.elapsed();
    info!(
        "Total validation time: {:?}, block processing rate: {:.2} (b/s), transaction processing rate: {:.2} (t/s)",
        elapsed,
        num_blocks as f64 / elapsed.as_secs_f64(),
        num_txs as f64 / elapsed.as_secs_f64(),
    );
}

fn submit_chunk(
    src_consensus: &Consensus,
    dst_consensus: &Consensus,
    chunk: &mut impl Iterator<Item = Hash>,
) -> Vec<impl Future<Output = BlockProcessResult<BlockStatus>>> {
    let mut futures = Vec::new();
    for hash in chunk {
        let block = Block::from_arcs(
            src_consensus.headers_store.get_header(hash).unwrap(),
            src_consensus.block_transactions_store.get(hash).unwrap(),
        );
        let f = dst_consensus.validate_and_insert_block(block);
        futures.push(f);
    }
    futures
}

fn topologically_ordered_hashes(src_consensus: &Consensus, genesis_hash: Hash) -> Vec<Hash> {
    let mut queue: VecDeque<Hash> = std::iter::once(genesis_hash).collect();
    let mut visited = BlockHashSet::new();
    let mut vec = Vec::new();
    let relations = src_consensus.relations_stores.read();
    while let Some(current) = queue.pop_front() {
        for child in relations[0].get_children(current).unwrap().iter() {
            if visited.insert(*child) {
                queue.push_back(*child);
                vec.push(*child);
            }
        }
    }
    vec.sort_by_cached_key(|&h| src_consensus.headers_store.get_timestamp(h).unwrap());
    vec
}

fn print_stats(src_consensus: &Consensus, hashes: &[Hash], delay: f64, bps: f64, k: KType) -> usize {
    let blues_mean =
        hashes.iter().map(|&h| src_consensus.ghostdag_primary_store.get_data(h).unwrap().mergeset_blues.len()).sum::<usize>() as f64
            / hashes.len() as f64;
    let reds_mean =
        hashes.iter().map(|&h| src_consensus.ghostdag_primary_store.get_data(h).unwrap().mergeset_reds.len()).sum::<usize>() as f64
            / hashes.len() as f64;
    let parents_mean = hashes.iter().map(|&h| src_consensus.headers_store.get_header(h).unwrap().direct_parents().len()).sum::<usize>()
        as f64
        / hashes.len() as f64;
    let num_txs = hashes.iter().map(|&h| src_consensus.block_transactions_store.get(h).unwrap().len()).sum::<usize>();
    let txs_mean = num_txs as f64 / hashes.len() as f64;
    info!("[DELAY={delay}, BPS={bps}, GHOSTDAG K={k}]");
    info!("[Average stats of generated DAG] blues: {blues_mean}, reds: {reds_mean}, parents: {parents_mean}, txs: {txs_mean}");
    num_txs
}
