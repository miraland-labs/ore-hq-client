use std::{
    ops::{ControlFlow, Range},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::prelude::*;
use clap::{arg, Parser};
use drillx::equix;
use futures_util::{SinkExt, StreamExt};
use rayon::prelude::*;
use solana_sdk::{signature::Keypair, signer::Signer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Once;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        handshake::client::{generate_key, Request},
        Message,
    },
};

static INIT_RAYON: Once = Once::new();

// Constants for tuning performance
const MIN_CHUNK_SIZE: u64 = 3_000_000;
const MAX_CHUNK_SIZE: u64 = 30_000_000;

#[derive(Debug)]
pub enum ServerMessage {
    StartMining([u8; 32], Range<u64>, u64),
}

#[derive(Debug, Parser)]
pub struct MineArgs {
    #[arg(
        long,
        value_name = "CORES",
        default_value = "1",
        help = "Number of cores to use while mining"
    )]
    pub cores: usize,
}

struct MiningResult {
    nonce: u64,
    difficulty: u32,
    hash: drillx::Hash,
    nonces_checked: u64,
}

// MI, add param start_nonce
impl MiningResult {
    fn new(start_nonce: u64) -> Self {
        MiningResult {
            // nonce: 0,
            nonce: start_nonce,
            difficulty: 0,
            hash: drillx::Hash::default(),
            nonces_checked: 0,
        }
    }
}

const MIN_DIFF: u32 = 5; // MI, align with server

fn calculate_dynamic_chunk_size(nonce_range: &Range<u64>, threads: usize) -> u64 {
    let range_size = nonce_range.end - nonce_range.start;
    let chunks_per_thread = 5;
    let ideal_chunk_size = range_size / (threads * chunks_per_thread) as u64;

    ideal_chunk_size.clamp(MIN_CHUNK_SIZE, MAX_CHUNK_SIZE)
}

fn optimized_mining_rayon(
    challenge: &[u8; 32],
    nonce_range: Range<u64>,
    cutoff_time: u64,
    cores: usize,
) -> (u64, u32, drillx::Hash, u64) {
    let stop_signal = Arc::new(AtomicBool::new(false));
    let total_nonces_checked = Arc::new(AtomicU64::new(0));

    // Initialize Rayon thread pool only once
    INIT_RAYON.call_once(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(cores)
            .build_global()
            .expect("Failed to initialize global thread pool");
    });

    let chunk_size = calculate_dynamic_chunk_size(&nonce_range, cores);
    let start_time = Instant::now();

    let results: Vec<MiningResult> = (0..cores)
        .into_par_iter()
        .map(|core_id| {
            let mut memory = equix::SolverMemory::new();
            let core_range_size = (nonce_range.end - nonce_range.start) / cores as u64;
            let core_start = nonce_range.start + core_id as u64 * core_range_size;
            let core_end = if core_id == cores - 1 {
                nonce_range.end
            } else {
                core_start + core_range_size
            };

            // let mut core_best = MiningResult::new();
            let mut core_best = MiningResult::new(core_start);
            let mut local_nonces_checked = 0;

            'outer: for chunk_start in (core_start..core_end).step_by(chunk_size as usize) {
                let chunk_end = (chunk_start + chunk_size).min(core_end);
                for nonce in chunk_start..chunk_end {
                    // MI, vanilla, duplicated with below % 100 part, cause default solution returned without any calc.
                    // if start_time.elapsed().as_secs() >= cutoff_time {
                    //     break 'outer;
                    // }

                    if stop_signal.load(Ordering::Relaxed) {
                        break 'outer;
                    }

                    for hx in
                        drillx::hashes_with_memory(&mut memory, challenge, &nonce.to_le_bytes())
                    {
                        local_nonces_checked += 1;
                        let difficulty = hx.difficulty();

                        if difficulty > core_best.difficulty {
                            core_best = MiningResult {
                                nonce,
                                difficulty,
                                hash: hx,
                                nonces_checked: local_nonces_checked,
                            };
                        }
                    }

                    if nonce % 100 == 0 && start_time.elapsed().as_secs() >= cutoff_time {
                        // if core_best.difficulty >= 8 {
                        if core_best.difficulty >= MIN_DIFF {
                            break 'outer;
                        }
                    }
                }
            }

            total_nonces_checked.fetch_add(local_nonces_checked, Ordering::Relaxed);
            core_best
        })
        .collect();

    stop_signal.store(true, Ordering::Relaxed);

    let best_result = results
        .into_iter()
        .reduce(|acc, x| {
            if x.difficulty > acc.difficulty {
                x
            } else {
                acc
            }
        })
        // .unwrap_or_else(MiningResult::new);
        .unwrap_or(MiningResult::new(nonce_range.start));

    (
        best_result.nonce,
        best_result.difficulty,
        best_result.hash,
        total_nonces_checked.load(Ordering::Relaxed),
    )
}

pub async fn mine(args: MineArgs, key: Keypair, url: String, unsecure: bool) {
    let mut cores = args.cores;
    let max_cores = core_affinity::get_core_ids().unwrap().len();
    if cores > max_cores {
        cores = max_cores
    }

    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs();

        let ts_msg = now.to_le_bytes();

        let sig = key.sign_message(&ts_msg);

        // MI: 172.21.235.113:3000
        let mut ws_url_str = if unsecure {
            format!("ws://{}", url)
        } else {
            format!("wss://{}", url)
        };

        if !ws_url_str.ends_with('/') {
            ws_url_str.push('/');
        }

        ws_url_str.push_str(&format!("?timestamp={}", now));
        let url = url::Url::parse(&ws_url_str).expect("Failed to parse server url");
        let host = url.host_str().expect("Invalid host in server url");

        let auth = BASE64_STANDARD.encode(format!("{}:{}", key.pubkey(), sig));

        println!("Connecting to server...");
        let request = Request::builder()
            .method("GET")
            .uri(url.to_string())
            .header("Sec-Websocket-Key", generate_key())
            .header("Host", host)
            .header("Upgrade", "websocket")
            .header("Connection", "upgrade")
            .header("Sec-Websocket-Version", "13")
            .header("Authorization", format!("Basic {}", auth))
            .body(())
            .unwrap();

        match connect_async(request).await {
            Ok((ws_stream, _)) => {
                println!("Connected to network!");

                let (mut sender, mut receiver) = ws_stream.split();
                let (message_sender, mut message_receiver) = unbounded_channel::<ServerMessage>();

                let receiver_thread = tokio::spawn(async move {
                    while let Some(Ok(message)) = receiver.next().await {
                        if process_message(message, message_sender.clone()).is_break() {
                            break;
                        }
                    }
                });

                // send Ready message
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_secs();

                let msg = now.to_le_bytes();
                let sig = key.sign_message(&msg).to_string().as_bytes().to_vec();
                let mut bin_data: Vec<u8> = Vec::with_capacity(1 + 32 + 8 + sig.len());
                bin_data.push(0u8);
                bin_data.extend_from_slice(&key.pubkey().to_bytes());
                bin_data.extend_from_slice(&msg);
                bin_data.extend(sig);

                let _ = sender.send(Message::Binary(bin_data)).await;

                // receive messages
                while let Some(msg) = message_receiver.recv().await {
                    match msg {
                        ServerMessage::StartMining(challenge, nonce_range, cutoff) => {
                            // // MI, intercept cutoff and reduce 1 sec for shceduling overhead
                            // let cutoff = if cutoff > 1 { cutoff - 1 } else { cutoff };
                            println!("Received start mining message!");
                            println!("Mining starting (Using Protomine)...");
                            println!("Nonce range: {} - {}", nonce_range.start, nonce_range.end);
                            let hash_timer = Instant::now();

                            let cutoff_time = cutoff; // Use the provided cutoff directly

                            let (best_nonce, best_difficulty, best_hash, total_nonces_checked) =
                                optimized_mining_rayon(&challenge, nonce_range, cutoff_time, cores);

                            let hash_time = hash_timer.elapsed();

                            println!("Found best diff: {}", best_difficulty);
                            println!("Processed: {}", total_nonces_checked);
                            println!("Hash time: {:?}", hash_time);
                            if hash_time.as_secs().gt(&0) {
                                println!(
                                    "Hashpower: {:?} H/s",
                                    total_nonces_checked.saturating_div(hash_time.as_secs())
                                );
                            } else {
                                println!("Hashpower: {:?} H/s", 0);
                            }

                            let message_type = 2u8; // 2 u8 - BestSolution Message
                            let best_hash_bin = best_hash.d; // 16 u8
                            let best_nonce_bin = best_nonce.to_le_bytes(); // 8 u8

                            let mut hash_nonce_message = [0; 24];
                            hash_nonce_message[0..16].copy_from_slice(&best_hash_bin);
                            hash_nonce_message[16..24].copy_from_slice(&best_nonce_bin);
                            let signature = key
                                .sign_message(&hash_nonce_message)
                                .to_string()
                                .as_bytes()
                                .to_vec();

                            let mut bin_data = Vec::with_capacity(57 + signature.len());
                            bin_data.extend_from_slice(&message_type.to_le_bytes());
                            bin_data.extend_from_slice(&best_hash_bin);
                            bin_data.extend_from_slice(&best_nonce_bin);
                            bin_data.extend_from_slice(&key.pubkey().to_bytes());
                            bin_data.extend(signature);

                            let _ = sender.send(Message::Binary(bin_data)).await;

                            tokio::time::sleep(Duration::from_secs(3)).await;

                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .expect("Time went backwards")
                                .as_secs();

                            let msg = now.to_le_bytes();
                            let sig = key.sign_message(&msg).to_string().as_bytes().to_vec();
                            let mut bin_data = Vec::with_capacity(1 + 32 + 8 + sig.len());
                            bin_data.push(0u8);
                            bin_data.extend_from_slice(&key.pubkey().to_bytes());
                            bin_data.extend_from_slice(&msg);
                            bin_data.extend(sig);

                            let _ = sender.send(Message::Binary(bin_data)).await;
                        }
                    }
                }

                let _ = receiver_thread.await;
            }
            Err(e) => {
                match e {
                    tokio_tungstenite::tungstenite::Error::Http(e) => {
                        if let Some(body) = e.body() {
                            eprintln!("Error: {:?}", String::from_utf8_lossy(body));
                        } else {
                            eprintln!("Http Error: {:?}", e);
                        }
                    }
                    _ => {
                        eprintln!("Error: {:?}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

fn process_message(
    msg: Message,
    message_channel: UnboundedSender<ServerMessage>,
) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t) => {
            println!("\n>>> Server Message: \n{}\n", t);
        }
        Message::Binary(b) => {
            let message_type = b[0];
            match message_type {
                0 => {
                    if b.len() < 49 {
                        println!("Invalid data for Message StartMining");
                    } else {
                        let mut hash_bytes = [0u8; 32];
                        // extract 256 bytes (32 u8's) from data for hash
                        let mut b_index = 1;
                        for i in 0..32 {
                            hash_bytes[i] = b[i + b_index];
                        }
                        b_index += 32;

                        // extract 64 bytes (8 u8's)
                        let mut cutoff_bytes = [0u8; 8];
                        for i in 0..8 {
                            cutoff_bytes[i] = b[i + b_index];
                        }
                        b_index += 8;
                        let cutoff = u64::from_le_bytes(cutoff_bytes);

                        let mut nonce_start_bytes = [0u8; 8];
                        for i in 0..8 {
                            nonce_start_bytes[i] = b[i + b_index];
                        }
                        b_index += 8;
                        let nonce_start = u64::from_le_bytes(nonce_start_bytes);

                        let mut nonce_end_bytes = [0u8; 8];
                        for i in 0..8 {
                            nonce_end_bytes[i] = b[i + b_index];
                        }
                        let nonce_end = u64::from_le_bytes(nonce_end_bytes);

                        let msg =
                            ServerMessage::StartMining(hash_bytes, nonce_start..nonce_end, cutoff);

                        let _ = message_channel.send(msg);
                    }
                }
                _ => {
                    println!("Failed to parse server message type");
                }
            }
        }
        Message::Ping(v) => {
            println!("Got Ping: {:?}", v);
        }
        Message::Pong(v) => {
            println!("Got Pong: {:?}", v);
        }
        Message::Close(v) => {
            println!("Got Close: {:?}", v);
            return ControlFlow::Break(());
        }
        _ => {
            println!("Got invalid message data");
        }
    }

    ControlFlow::Continue(())
}
