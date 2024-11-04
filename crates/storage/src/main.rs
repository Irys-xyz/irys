use actix::prelude::*;
use bytes::Bytes;
use futures::future::join_all;
use std::sync::Arc;
use std::time::{Duration, Instant};

enum LargeMessage {
    OwnedBinary(Vec<u8>),
    SharedBinary(Bytes),
    ArcBinary(Arc<Vec<u8>>),
    ArcBytes(Arc<Bytes>), // New variant
}

#[derive(Debug, MessageResponse)]
struct ProcessingResult {
    bytes_processed: usize,
    processing_time: Duration,
    checksum: u64,
}

impl Message for LargeMessage {
    type Result = ProcessingResult;
}

struct BinaryProcessor {
    processed_count: usize,
    start_time: Instant,
}

impl Actor for BinaryProcessor {
    type Context = Context<Self>;
}

impl Handler<LargeMessage> for BinaryProcessor {
    type Result = ProcessingResult;

    fn handle(&mut self, msg: LargeMessage, _ctx: &mut Context<Self>) -> ProcessingResult {
        let start = Instant::now();
        self.processed_count += 1;

        let (bytes_processed, checksum) = match msg {
            LargeMessage::OwnedBinary(data) => {
                println!("Processing owned binary of size: {}", data.len());
                let sum = data.iter().enumerate().fold(0u64, |acc, (i, &byte)| {
                    acc.wrapping_add(byte as u64 * i as u64)
                });
                (data.len(), sum)
            }
            LargeMessage::SharedBinary(data) => {
                println!("Processing shared binary of size: {}", data.len());
                let sum = data.iter().enumerate().fold(0u64, |acc, (i, byte)| {
                    acc.wrapping_add(*byte as u64 * i as u64)
                });
                (data.len(), sum)
            }
            LargeMessage::ArcBinary(data) => {
                println!("Processing Arc binary of size: {}", data.len());
                let sum = data.iter().enumerate().fold(0u64, |acc, (i, &byte)| {
                    acc.wrapping_add(byte as u64 * i as u64)
                });
                (data.len(), sum)
            }
            LargeMessage::ArcBytes(data) => {
                println!("Processing Arc<Bytes> of size: {}", data.len());
                let sum = data.iter().enumerate().fold(0u64, |acc, (i, byte)| {
                    acc.wrapping_add(*byte as u64 * i as u64)
                });
                (data.len(), sum)
            }
        };

        ProcessingResult {
            bytes_processed,
            processing_time: start.elapsed(),
            checksum,
        }
    }
}

async fn process_batch<F>(
    processor: &Addr<BinaryProcessor>,
    message_factory: F,
    batch_size: usize,
    message_type: &str,
) -> Vec<ProcessingResult>
where
    F: Fn() -> LargeMessage,
{
    println!(
        "\nStarting batch of {} {} messages...",
        batch_size, message_type
    );
    let start = Instant::now();

    let futures: Vec<_> = (0..batch_size)
        .map(|_| processor.send(message_factory()))
        .collect();

    let results = join_all(futures)
        .await
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();

    let total_time = start.elapsed();
    let total_bytes: usize = results.iter().map(|r| r.bytes_processed).sum();
    let avg_processing_time: Duration = if !results.is_empty() {
        results.iter().map(|r| r.processing_time).sum::<Duration>() / results.len() as u32
    } else {
        Duration::default()
    };

    let first_checksum = results.first().map(|r| r.checksum);
    let all_match = results.iter().all(|r| Some(r.checksum) == first_checksum);

    println!("Batch results for {}:", message_type);
    println!("  Total messages processed: {}", results.len());
    println!("  Total bytes processed: {} MB", total_bytes / 1024 / 1024);
    println!("  Total batch time: {:?}", total_time);
    println!(
        "  Average message processing time: {:?}",
        avg_processing_time
    );
    println!(
        "  Throughput: {:.2} messages/second",
        batch_size as f64 / total_time.as_secs_f64()
    );
    println!(
        "  Throughput: {:.2} MB/second",
        (total_bytes as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64()
    );
    println!("  Checksums match: {}", all_match);

    results
}

#[actix_rt::main]
async fn main() {
    let processor = BinaryProcessor {
        processed_count: 0,
        start_time: Instant::now(),
    }
    .start();

    // Test parameters
    let data_size = 10 * 1024 * 1024; // 10MB
    let batch_size = 100;

    // Create test data with some varying content
    let large_data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();

    // Test owned binary messages
    let owned_results = process_batch(
        &processor,
        || LargeMessage::OwnedBinary(large_data.clone()),
        batch_size,
        "Owned Binary",
    )
    .await;

    // Test shared binary messages
    let shared_data = Bytes::from(large_data.clone());
    let shared_results = process_batch(
        &processor,
        || LargeMessage::SharedBinary(shared_data.clone()),
        batch_size,
        "Shared Binary",
    )
    .await;

    // Test Arc binary messages
    let arc_data = Arc::new(large_data.clone());
    let arc_results = process_batch(
        &processor,
        || LargeMessage::ArcBinary(arc_data.clone()),
        batch_size,
        "Arc Binary",
    )
    .await;

    // Test Arc<Bytes> messages
    let arc_bytes_data = Arc::new(Bytes::from(large_data));
    let arc_bytes_results = process_batch(
        &processor,
        || LargeMessage::ArcBytes(arc_bytes_data.clone()),
        batch_size,
        "Arc<Bytes>",
    )
    .await;

    println!("\nComparative Summary:");
    println!("1. Owned Binary:");
    println!(
        "   - Total processing time: {:?}",
        owned_results
            .iter()
            .map(|r| r.processing_time)
            .sum::<Duration>()
    );
    println!(
        "   - Average MB/s: {:.2}",
        owned_results
            .iter()
            .map(|r| r.bytes_processed as f64 / 1024.0 / 1024.0 / r.processing_time.as_secs_f64())
            .sum::<f64>()
            / owned_results.len() as f64
    );

    println!("2. Shared Binary (Bytes):");
    println!(
        "   - Total processing time: {:?}",
        shared_results
            .iter()
            .map(|r| r.processing_time)
            .sum::<Duration>()
    );
    println!(
        "   - Average MB/s: {:.2}",
        shared_results
            .iter()
            .map(|r| r.bytes_processed as f64 / 1024.0 / 1024.0 / r.processing_time.as_secs_f64())
            .sum::<f64>()
            / shared_results.len() as f64
    );

    println!("3. Arc Binary (Arc<Vec<u8>>):");
    println!(
        "   - Total processing time: {:?}",
        arc_results
            .iter()
            .map(|r| r.processing_time)
            .sum::<Duration>()
    );
    println!(
        "   - Average MB/s: {:.2}",
        arc_results
            .iter()
            .map(|r| r.bytes_processed as f64 / 1024.0 / 1024.0 / r.processing_time.as_secs_f64())
            .sum::<f64>()
            / arc_results.len() as f64
    );

    println!("4. Arc<Bytes>:");
    println!(
        "   - Total processing time: {:?}",
        arc_bytes_results
            .iter()
            .map(|r| r.processing_time)
            .sum::<Duration>()
    );
    println!(
        "   - Average MB/s: {:.2}",
        arc_bytes_results
            .iter()
            .map(|r| r.bytes_processed as f64 / 1024.0 / 1024.0 / r.processing_time.as_secs_f64())
            .sum::<f64>()
            / arc_bytes_results.len() as f64
    );

    System::current().stop();
}
