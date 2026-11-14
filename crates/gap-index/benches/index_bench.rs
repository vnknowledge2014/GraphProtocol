//! Microbenchmark for `gap-index` hybrid BM25 + SIMD flat scan + RRF queries (`RTE-001`, `RTE-006`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

use gap_core::ids::TenantId;
use gap_core::traits::{ElementKind, ElementSearch};
use gap_index::{ElementDoc, HybridElementIndex, HybridIndexConfig, normalize_l2};

fn synth_vec(dim: usize, seed: u64) -> Vec<f32> {
    let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::with_capacity(dim);
    for _ in 0..dim {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        out.push(((s & 0xFFFF) as f32 / 32_768.0) - 1.0);
    }
    let _ = normalize_l2(&mut out);
    out
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let quick = args.iter().any(|a| a == "--quick");
    let iterations = if quick { 25_usize } else { 200_usize };
    let dim = 384_usize;
    let count = 1_000_usize;

    let mut docs = Vec::with_capacity(count);
    for i in 0..count {
        docs.push(
            ElementDoc::new(
                format!("branch{}.getOrder_{i:04}", i % 10),
                ElementKind::Operation,
                format!("getCustomerOrder_{i}"),
            )
            .with_path(format!("commerce.orders.item{i}"))
            .with_text(
                "Fetches customer order line items, invoice status, and fulfillment tracking",
            )
            .with_examples(vec!["query { order(id: \"123\") { status } }".to_owned()])
            .with_embedding(synth_vec(dim, (i as u64) + 1)),
        );
    }

    let index = match HybridElementIndex::build(docs, HybridIndexConfig::default()) {
        Ok(idx) => idx,
        Err(err) => {
            eprintln!("index_bench build failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    let scope = TenantId::default_tenant().scope();
    let q_vec = synth_vec(dim, 42);
    let start = Instant::now();
    for iter in 0..iterations {
        let query = if iter % 2 == 0 {
            "customer order invoice"
        } else {
            "fulfillment tracking status"
        };
        match index.search(
            black_box(&scope),
            black_box(query),
            black_box(Some(&q_vec)),
            black_box(10),
        ) {
            Ok(hits) => {
                let _ = black_box(hits.len());
            }
            Err(err) => {
                eprintln!("index_bench query failed: {err}");
                return ExitCode::FAILURE;
            }
        }
    }
    let elapsed = start.elapsed();
    let per_query_us = elapsed.as_secs_f64() * 1_000_000.0 / (iterations as f64);
    println!(
        "index_bench (elements={count}, dim={dim}, iters={iterations}, quick={quick}): total_ms={:.2}, per_query_us={per_query_us:.2}",
        elapsed.as_secs_f64() * 1_000.0
    );

    ExitCode::SUCCESS
}
