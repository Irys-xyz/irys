use std::fs::File;

use clap::Parser;
use irys_reward_curve::HalvingCurve;
use irys_types::{
    storage_pricing::{safe_add, Amount},
    U256,
};

/// Command‑line parameters for the simulation.
#[derive(Parser, Debug)]
#[command(name = "Emission Simulator", author, version, about)]
struct Args {
    /// Block interval in seconds.
    #[arg(long = "block-interval-secs", default_value_t = 1200)]
    block_interval_secs: u128,

    /// Number of whole years to simulate (from genesis).
    #[arg(long = "years", default_value_t = 20)]
    years: u128,

    /// Path to the CSV output file.
    #[arg(long = "output", default_value = "emission_simulation.csv")]
    output: String,
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();

    const INFLATION_CAP_TOKENS: u128 = 100_000_000;
    const HALF_LIFE_YEARS: u128 = 4;
    const SECS_PER_YEAR: u128 = 31_557_600;

    let curve = HalvingCurve {
        inflation_cap: Amount::token(INFLATION_CAP_TOKENS.into()).unwrap(),
        half_life_secs: HALF_LIFE_YEARS * SECS_PER_YEAR,
    };

    let total_seconds = args.years * SECS_PER_YEAR;
    let mut prev_ts: u128 = 0;
    let mut block_number: u64 = 0;
    let mut cumulative: U256 = U256::zero();

    let mut wtr = csv::Writer::from_writer(File::create(&args.output)?);
    wtr.write_record(&[
        "block_number",
        "timestamp_secs",
        "emission_per_block",
        "cumulative_emission",
    ])?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(move |s| {
        s.spawn(move || {
            while prev_ts < total_seconds {
                let next_ts = (prev_ts + args.block_interval_secs).min(total_seconds);
                let reward = curve.reward_between(prev_ts, next_ts).unwrap();
                cumulative = safe_add(cumulative, reward.amount).unwrap();

                tx.send([
                    block_number.to_string(),
                    next_ts.to_string(),
                    reward.token_to_decimal().unwrap().to_string(),
                    Amount::<()>::new(cumulative)
                        .token_to_decimal()
                        .unwrap()
                        .to_string(),
                ])
                .unwrap();

                prev_ts = next_ts;
                block_number = block_number.saturating_add(1);
            }
        });

        let mut last_pct: u8 = 0;
        while let Ok(res) = rx.recv() {
            wtr.write_record(&res).unwrap();

            // progress calculation: timestamp_secs is at index 1
            let ts_secs: u128 = res[1].parse().unwrap();
            let pct = ((ts_secs * 100) / total_seconds) as u8;

            if pct > last_pct {
                last_pct = pct;
                println!("Progress: {pct}%");
            }
        }
        wtr.flush().unwrap();
    });

    println!("CSV written to {}", args.output);
    Ok(())
}
