use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use serde::Serialize;
use std::time::Duration;

use crate::common::{
    OutputMode, compute_cost, format_walltime_slurm, parse_memory, parse_walltime, slurm_cmd,
};

/// Estimate the dollar cost of a hypothetical job.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Partition name.
    #[arg(short, long, default_value = "standard")]
    pub partition: String,

    /// Total cores.
    #[arg(short, long, default_value_t = 1)]
    pub cores: u32,

    /// Total GPUs.
    #[arg(short, long, default_value_t = 0)]
    pub gpus: u32,

    /// Total nodes.
    #[arg(short, long, default_value_t = 1)]
    pub nodes: u32,

    /// Total memory with unit (e.g., 10g, 768mb, 1t).
    #[arg(short, long, default_value = "768mb")]
    pub memory: String,

    /// Walltime (DD-HH:MM:SS, HH:MM:SS, MM:SS, etc).
    #[arg(short, long, default_value = "01:00:00")]
    pub time: String,
}

struct BillingWeights {
    cpu: f64,
    mem: f64,
    gpu: Option<f64>,
}

#[derive(Serialize)]
struct JobEstimateJson {
    module: &'static str,
    version: &'static str,
    cluster: Option<String>,
    partition: String,
    nodes: u32,
    cores: u32,
    gpus: u32,
    memory_bytes: u64,
    walltime_seconds: u64,
    walltime_slurm: String,
    billing_weights: BillingWeightsJson,
    billing_value: f64,
    cost_dollars: f64,
}

#[derive(Serialize)]
struct BillingWeightsJson {
    cpu: f64,
    mem: f64,
    gpu: Option<f64>,
}

const BILLING_DIVISOR: u64 = 10_000_000;

pub async fn run(args: &Args, output_mode: OutputMode) -> Result<()> {
    // Validate GPU / partition consistency
    let has_gpus = args.gpus > 0;
    let is_gpu_partition = args.partition.contains("gpu");

    if has_gpus && !is_gpu_partition {
        bail!("cannot request GPUs with the {} partition", args.partition);
    }
    if is_gpu_partition && !has_gpus {
        bail!(
            "cannot request the {} partition without requesting at least 1 GPU",
            args.partition
        );
    }

    // Parse memory and walltime
    let mem_bytes = parse_memory(&args.memory, 1).context("parsing --memory")?;
    let duration = parse_walltime(&args.time).context("parsing --time")?;

    // Memory in GiB for billing formula (Slurm uses per-GiB weights)
    let mem_gib = mem_bytes as f64 / (1u64 << 30) as f64;

    // Total minutes (fractional)
    let total_minutes = duration.as_secs_f64() / 60.0;

    // Fetch billing weights from scontrol
    let weights = fetch_billing_weights(&args.partition, is_gpu_partition)
        .await
        .with_context(|| {
            format!(
                "fetching billing weights for partition '{}'",
                args.partition
            )
        })?;

    // Billing = max(cpu_w * cpus, mem_w * mem_GiB [, gpu_w * gpus])
    let mut billing = f64::max(weights.cpu * args.cores as f64, weights.mem * mem_gib);
    if let Some(gpu_w) = weights.gpu {
        billing = billing.max(gpu_w * args.gpus as f64);
    }

    // Cost = (total_minutes * billing) / 10,000,000
    let cost = compute_cost(billing, total_minutes, BILLING_DIVISOR);

    // Output
    if output_mode.is_json() {
        print_json(args, mem_bytes, &duration, &weights, billing, cost);
    } else {
        print_summary(args, mem_bytes, &duration, cost, total_minutes);
    }

    Ok(())
}

async fn fetch_billing_weights(partition: &str, is_gpu: bool) -> Result<BillingWeights> {
    let output = slurm_cmd(&["scontrol", "show", "partition", partition])
        .await
        .map_err(|e| anyhow::anyhow!("scontrol failed: {e}"))?;

    // Find TRESBillingWeights=... in the output
    let weights_str = extract_tres_billing_weights(&output)
        .ok_or_else(|| anyhow::anyhow!("invalid partition: {partition}"))?;

    parse_billing_weights(weights_str, is_gpu)
}

/// Extract the TRESBillingWeights value from scontrol output.
fn extract_tres_billing_weights(output: &str) -> Option<&str> {
    for part in output.split_whitespace() {
        if let Some(val) = part.strip_prefix("TRESBillingWeights=") {
            if val.is_empty() {
                return None;
            }
            return Some(val);
        }
    }
    None
}

/// Parse `CPU=1.0,Mem=0.125G,GRES/gpu=100` into `BillingWeights`.
fn parse_billing_weights(s: &str, is_gpu: bool) -> Result<BillingWeights> {
    let mut cpu = 0.0_f64;
    let mut mem = 0.0_f64;
    let mut gpu = None;

    for kv in s.split(',') {
        let (key, val) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid billing weight entry: '{kv}'"))?;

        let key_lower = key.to_ascii_lowercase();
        // Strip trailing unit suffix from value (e.g., "0.25G" → "0.25")
        let val_clean = val.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        let v: f64 = val_clean
            .parse()
            .with_context(|| format!("invalid billing weight value: '{val}'"))?;

        if key_lower == "cpu" {
            cpu = v;
        } else if key_lower == "mem" {
            mem = v;
        } else if key_lower.starts_with("gres/gpu") {
            gpu = Some(v);
        }
    }

    if is_gpu && gpu.is_none() {
        bail!("partition has no GPU billing weight");
    }

    Ok(BillingWeights { cpu, mem, gpu })
}

fn print_summary(args: &Args, mem_bytes: u64, duration: &Duration, cost: f64, total_minutes: f64) {
    let total_secs = duration.as_secs();
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    let mem_mib = mem_bytes as f64 / (1u64 << 20) as f64;

    println!("--------------------------------------------------");
    println!("Job Detail Summary:");
    println!("--------------------------------------------------");
    println!("Partition:     {}", args.partition);
    println!("Total Nodes:   {}", args.nodes);
    println!("Total Cores:   {}", args.cores);
    println!("Total Memory:  {} MiB", mem_mib as u64);
    if args.gpus > 0 {
        println!("Total GPUs:    {}", args.gpus);
    }
    println!();
    println!("Walltime:      {} day(s)", days);
    println!("               {} hour(s)", hours);
    println!("               {} minute(s)", mins);
    println!("               {} second(s)", secs);
    println!("--------------------------------------------------");
    println!("Cost Estimate:");
    println!("--------------------------------------------------");
    let cost_rounded = format!("{:.2}", cost);
    let cost_raw = format!("{}", cost);
    let total_hours = total_minutes / 60.0;
    println!(
        "Total:  ${cost_rounded} (${cost_raw}) for {} hours.",
        total_hours
    );
    println!();
    println!("NOTE: This price estimate assumes your job runs");
    println!("for the full walltime. Cost is subject to change.");
}

fn print_json(
    args: &Args,
    mem_bytes: u64,
    duration: &Duration,
    weights: &BillingWeights,
    billing: f64,
    cost: f64,
) {
    let cluster = std::env::var("CLUSTER_NAME").ok();
    let json = JobEstimateJson {
        module: "job_estimate",
        version: env!("CARGO_PKG_VERSION"),
        cluster,
        partition: args.partition.clone(),
        nodes: args.nodes,
        cores: args.cores,
        gpus: args.gpus,
        memory_bytes: mem_bytes,
        walltime_seconds: duration.as_secs(),
        walltime_slurm: format_walltime_slurm(*duration),
        billing_weights: BillingWeightsJson {
            cpu: weights.cpu,
            mem: weights.mem,
            gpu: weights.gpu,
        },
        billing_value: billing,
        cost_dollars: cost,
    };

    println!("{}", serde_json::to_string_pretty(&json).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_tres_billing_weights() {
        let output = "PartitionName=standard\n   AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL\n   TRESBillingWeights=CPU=1.0,Mem=0.25G\n   DefMemPerCPU=UNLIMITED";
        assert_eq!(
            extract_tres_billing_weights(output),
            Some("CPU=1.0,Mem=0.25G")
        );
    }

    #[test]
    fn test_extract_tres_billing_weights_empty() {
        let output = "PartitionName=debug\n   TRESBillingWeights=\n   Other=stuff";
        assert!(extract_tres_billing_weights(output).is_none());
    }

    #[test]
    fn test_parse_billing_weights_no_gpu() {
        let w = parse_billing_weights("CPU=1.0,Mem=0.25G", false).unwrap();
        assert!((w.cpu - 1.0).abs() < f64::EPSILON);
        assert!((w.mem - 0.25).abs() < f64::EPSILON);
        assert!(w.gpu.is_none());
    }

    #[test]
    fn test_parse_billing_weights_with_gpu() {
        let w = parse_billing_weights("CPU=1.0,Mem=0.125G,GRES/gpu=100", true).unwrap();
        assert!((w.cpu - 1.0).abs() < f64::EPSILON);
        assert!((w.mem - 0.125).abs() < f64::EPSILON);
        assert!((w.gpu.unwrap() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_billing_weights_gpu_partition_missing_gpu_weight() {
        let result = parse_billing_weights("CPU=1.0,Mem=0.25G", true);
        assert!(result.is_err());
    }

    #[test]
    fn test_billing_calculation_no_gpu() {
        // billing = max(1.0 * 4, 0.25 * 0.75) = max(4.0, 0.1875) = 4.0
        // cost = (60.0 * 4.0) / 10_000_000 = 0.000024
        let billing = f64::max(1.0 * 4.0, 0.25 * 0.75);
        let cost = compute_cost(billing, 60.0, BILLING_DIVISOR);
        assert!((billing - 4.0).abs() < f64::EPSILON);
        assert!((cost - 0.000024).abs() < 1e-10);
    }

    #[test]
    fn test_billing_calculation_with_gpu() {
        // billing = max(1.0 * 4, 0.125 * 10.0, 100.0 * 2) = max(4.0, 1.25, 200.0) = 200.0
        // cost = (60.0 * 200.0) / 10_000_000 = 0.0012
        let billing = f64::max(f64::max(1.0 * 4.0, 0.125 * 10.0), 100.0 * 2.0);
        let cost = compute_cost(billing, 60.0, BILLING_DIVISOR);
        assert!((billing - 200.0).abs() < f64::EPSILON);
        assert!((cost - 0.0012).abs() < 1e-10);
    }
}
