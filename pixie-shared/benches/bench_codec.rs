use criterion::{Criterion, Throughput};
use pixie_shared::{
    MAX_CHUNK_SIZE, UDP_BODY_LEN,
    chunk_codec::{Decoder, Encoder},
};
use rand::{Rng, RngCore, SeedableRng, rngs::StdRng, seq::index};
use std::hint::black_box;
use std::time::Duration;

fn make_chunk(size: usize) -> Vec<u8> {
    let mut chunk = vec![0u8; size];
    StdRng::seed_from_u64(0x1234_5678_9ABC_DEF0).fill_bytes(&mut chunk);
    chunk
}

fn encode_all(chunk: &[u8]) -> Vec<Vec<u8>> {
    let mut encoder = Encoder::new(chunk.to_vec());
    let mut packets = Vec::new();
    let mut buf = [0u8; UDP_BODY_LEN];
    while let Some(len) = encoder.next_packet(&mut buf) {
        packets.push(buf[..len].to_vec());
    }
    packets
}

fn run_speed_benchmarks(c: &mut Criterion, chunk_4m: &[u8], packets_4m: &[Vec<u8>]) {
    let mut group = c.benchmark_group("chunk_codec");
    group.sample_size(60);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));

    for (name, size) in [
        ("roundtrip_20b", 20usize),
        ("roundtrip_200kib", 200 << 10),
        ("roundtrip_4mib", MAX_CHUNK_SIZE),
    ] {
        let chunk = make_chunk(size);
        let mut buf = [0u8; UDP_BODY_LEN];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut encoder = Encoder::new(black_box(&chunk).clone());
                let mut decoder = Decoder::new(chunk.len());
                while let Some(len) = encoder.next_packet(&mut buf) {
                    decoder
                        .add_packet(&buf[..len])
                        .expect("failed to add packet");
                }
                black_box(decoder.finish().expect("failed to decode chunk"))
            });
        });
    }

    group.throughput(Throughput::Bytes(chunk_4m.len() as u64));
    let mut buf = [0u8; UDP_BODY_LEN];
    group.bench_function("encode_4mib", |b| {
        b.iter(|| {
            let mut encoder = Encoder::new(black_box(chunk_4m).to_vec());
            while let Some(len) = encoder.next_packet(&mut buf) {
                black_box(&buf[..len]);
            }
        });
    });

    let decode_scenarios: &[(&str, Vec<usize>)] = &[
        ("decode_0_drops", vec![]),
        ("decode_1_drop", vec![100]),
        ("decode_4_drops_stride32", (0..4).map(|i| i * 32).collect()),
        (
            "decode_16_drops_stride32",
            (0..16).map(|i| i * 32).collect(),
        ),
        ("decode_16_drops_burst", (500..516).collect()),
        ("decode_32_drops_burst", (500..532).collect()),
        (
            "decode_32_drops_stride32",
            (0..32).map(|i| i * 32).collect(),
        ),
    ];

    for (name, dropped_indices) in decode_scenarios {
        let mut is_dropped = vec![false; packets_4m.len()];
        for &idx in dropped_indices {
            is_dropped[idx] = true;
        }
        group.bench_function(*name, |b| {
            b.iter(|| {
                let mut decoder = Decoder::new(chunk_4m.len());
                for (idx, pkt) in packets_4m.iter().enumerate() {
                    if !is_dropped[idx] {
                        decoder.add_packet(pkt).unwrap();
                    }
                }
                let decoded = decoder.finish().expect("failed to decode chunk");
                assert_eq!(decoded.len(), chunk_4m.len());
                black_box(decoded)
            });
        });
    }

    group.finish();
}

fn test_drop_pattern(chunk: &[u8], packets: &[Vec<u8>], is_dropped: &[bool]) -> bool {
    let mut decoder = Decoder::new(chunk.len());
    for (idx, pkt) in packets.iter().enumerate() {
        if !is_dropped[idx] {
            decoder.add_packet(pkt).unwrap();
        }
    }
    if let Some(decoded) = decoder.finish() {
        assert_eq!(decoded, chunk);
        true
    } else {
        false
    }
}

fn run_recovery_benchmarks(chunk: &[u8], packets: &[Vec<u8>]) {
    let total = packets.len();
    let num_data = total - 32;
    let trials = 100;

    println!(
        "\n=== Single-Pass Recovery Rate ({:.1} MiB chunk, {} packets, {} trials/row) ===",
        chunk.len() as f64 / (1024.0 * 1024.0),
        total,
        trials
    );
    println!(
        "{:>5} | {:>16} | {:>16} | {:>16}",
        "Drops", "Random Drops", "Contiguous Burst", "Stride-32 Drops"
    );
    println!("{:-<5}-+-{:-<16}-+-{:-<16}-+-{:-<16}", "", "", "", "");

    for &k in &[1usize, 2, 4, 6, 8, 12, 16, 20, 24, 28, 30, 32] {
        // 1. Random drops across all packets (data + parity)
        let mut rng = StdRng::seed_from_u64(0xCAFE_BABE_0000 + k as u64);
        let mut random_ok = 0;
        let mut is_dropped = vec![false; total];
        for _ in 0..trials {
            is_dropped.fill(false);
            for idx in index::sample(&mut rng, total, k) {
                is_dropped[idx] = true;
            }
            if test_drop_pattern(chunk, packets, &is_dropped) {
                random_ok += 1;
            }
        }

        // 2. Contiguous burst drops of length k across data packets
        let mut burst_ok = 0;
        for t in 0..trials {
            is_dropped.fill(false);
            let start = (t * 29) % (num_data - k);
            for slot in &mut is_dropped[start..start + k] {
                *slot = true;
            }
            if test_drop_pattern(chunk, packets, &is_dropped) {
                burst_ok += 1;
            }
        }

        // 3. Stride-32 drops of length k
        let mut stride_ok = 0;
        for t in 0..trials {
            is_dropped.fill(false);
            let start = (t * 17) % (num_data - (k - 1) * 32);
            for i in 0..k {
                is_dropped[start + i * 32] = true;
            }
            if test_drop_pattern(chunk, packets, &is_dropped) {
                stride_ok += 1;
            }
        }

        println!(
            "{:>5} | {:>15.1}% | {:>15.1}% | {:>15.1}%",
            k,
            100.0 * random_ok as f64 / trials as f64,
            100.0 * burst_ok as f64 / trials as f64,
            100.0 * stride_ok as f64 / trials as f64,
        );
    }
    println!();
}

fn run_multipass_simulation(chunk: &[u8], packets: &[Vec<u8>]) {
    let total = packets.len();
    let num_data = total - 32;
    let trials = 50;

    println!(
        "=== Multi-Pass Broadcast Simulation ({:.1} MiB chunk, {} data + 32 parity packets, {} trials/row) ===",
        chunk.len() as f64 / (1024.0 * 1024.0),
        num_data,
        trials
    );
    println!(
        "{:>10} | {:>14} | {:>14} | {:>18}",
        "Loss Rate", "1-Pass Success", "Avg Passes", "Avg Rx Packets"
    );
    println!("{:-<10}-+-{:-<14}-+-{:-<14}-+-{:-<18}", "", "", "", "");

    for &loss_pct in &[0.05f64, 0.1, 0.25, 0.5, 0.75, 1.0, 1.5] {
        let mut rng = StdRng::seed_from_u64(0xDEAD_BEEF + (loss_pct * 1000.0) as u64);
        let drop_prob = loss_pct / 100.0;
        let mut first_pass_ok = 0;
        let mut total_passes = 0usize;
        let mut total_rx_packets = 0usize;

        for _ in 0..trials {
            let mut decoder = Decoder::new(chunk.len());
            let mut passes = 0usize;
            let mut rx_packets = 0usize;
            let decoded = 'outer: loop {
                passes += 1;
                for pkt in packets {
                    if !rng.random_bool(drop_prob) {
                        rx_packets += 1;
                        decoder.add_packet(pkt).unwrap();
                        if let Some(data) = decoder.finish() {
                            break 'outer data;
                        }
                    }
                }
            };
            assert_eq!(decoded, chunk);
            if passes == 1 {
                first_pass_ok += 1;
            }
            total_passes += passes;
            total_rx_packets += rx_packets;
        }

        println!(
            "{:>9.2}% | {:>13.1}% | {:>14.2} | {:>18.1}",
            loss_pct,
            100.0 * first_pass_ok as f64 / trials as f64,
            total_passes as f64 / trials as f64,
            total_rx_packets as f64 / trials as f64,
        );
    }
}

fn main() {
    let chunk_4m = make_chunk(MAX_CHUNK_SIZE);
    let packets_4m = encode_all(&chunk_4m);

    let mut criterion = Criterion::default().configure_from_args();
    run_speed_benchmarks(&mut criterion, &chunk_4m, &packets_4m);
    criterion.final_summary();

    run_recovery_benchmarks(&chunk_4m, &packets_4m);
    run_multipass_simulation(&chunk_4m, &packets_4m);
}
