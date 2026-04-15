use std::env;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use colored::Colorize;
use serde::Serialize;

use crate::common::{
    Align, ClusterEnv, Column, OutputMode, SpinnerGroup, SpinnerKind, Table, color_dim,
    color_success, parse_memory, slurm_cmd, slurm_cmd_parallel, validate_account,
};

/// Arguments for `myrc account running`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Account to query.
    #[arg(short, long)]
    pub account: String,

    /// Per-job breakdown: user, jobid, nodes, cores, gpus, memory.
    #[arg(short = 'd', long = "detail")]
    pub detail: bool,
}

#[derive(Debug, Serialize)]
struct AccountRunningJson {
    module: &'static str,
    version: &'static str,
    account: String,
    cluster: Option<String>,
    totals: TotalsJson,
    jobs: Vec<JobJson>,
}

#[derive(Debug, Serialize)]
struct TotalsJson {
    jobs: usize,
    cores: u64,
    gpus: u64,
    memory_bytes: u64,
    nodes: u64,
}

#[derive(Debug, Serialize)]
struct JobJson {
    job_id: String,
    user: String,
    nodes: u64,
    cores: u64,
    gpus: u64,
    memory_bytes: u64,
}

struct JobInfo {
    job_id: String,
    user: String,
    nodes: u64,
    cores: u64,
    gpus: u64,
    memory_bytes: u64,
}

pub async fn run(args: &Args, output_mode: OutputMode) -> Result<()> {
    let cluster = ClusterEnv::from_env().ok();

    // Phase 1: validate account
    if !output_mode.is_json() {
        eprint!("{}", "Validating account...".bold());
    }
    validate_account(&args.account)
        .await
        .context("validating account")?;
    if !output_mode.is_json() {
        eprintln!(" {}", color_success("Done."));
    }

    // Phase 2: list running jobs
    let squeue_output = slurm_cmd(&["squeue", "-t", "r", "-h", "-o", "%i", "-A", &args.account])
        .await
        .context("listing running jobs")?;

    let job_ids: Vec<&str> = squeue_output
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    if job_ids.is_empty() {
        if output_mode.is_json() {
            let json = AccountRunningJson {
                module: "account_running",
                version: env!("CARGO_PKG_VERSION"),
                account: args.account.clone(),
                cluster: cluster.map(|c| c.name),
                totals: TotalsJson {
                    jobs: 0,
                    cores: 0,
                    gpus: 0,
                    memory_bytes: 0,
                    nodes: 0,
                },
                jobs: vec![],
            };
            println!("{}", serde_json::to_string_pretty(&json)?);
        } else {
            eprintln!(
                "{}",
                color_dim(&format!("No running jobs for account {}.", args.account))
            );
        }
        return Ok(());
    }

    // Phase 3: concurrent scontrol queries
    let num_jobs = job_ids.len();
    if !output_mode.is_json() {
        eprintln!("\n{}", format!("Querying {} jobs...", num_jobs).bold());
    }

    let mut spinner_group = SpinnerGroup::new();
    let total_spinner;
    let success_spinner;
    if !output_mode.is_json() {
        total_spinner = Some(spinner_group.add(SpinnerKind::Total, "Total:").clone());
        success_spinner = Some(spinner_group.add(SpinnerKind::Success, "Success:").clone());
    } else {
        total_spinner = None;
        success_spinner = None;
    }
    if let Some(ref sp) = total_spinner {
        sp.set_message(format!("0/{num_jobs} jobs"));
    }

    let cmds: Vec<Vec<String>> = job_ids
        .iter()
        .map(|id| {
            vec![
                "scontrol".to_string(),
                "show".to_string(),
                "job".to_string(),
                id.to_string(),
            ]
        })
        .collect();

    let results = slurm_cmd_parallel(cmds).await?;

    if let Some(ref sp) = total_spinner {
        sp.set_message(format!("{num_jobs}/{num_jobs} jobs"));
    }
    if let Some(ref sp) = success_spinner {
        sp.set_message(format!("{num_jobs}/{num_jobs} jobs"));
    }
    spinner_group.finish();

    if !output_mode.is_json() {
        eprintln!("{}\n", color_success("Done."));
    }

    // Phase 4: parse scontrol output
    let mut jobs: Vec<JobInfo> = Vec::with_capacity(num_jobs);

    for (i, output) in results.iter().enumerate() {
        let job_id = job_ids[i].to_string();
        let (user, nodes, cores, gpus, mem_bytes) = parse_scontrol_job(output);
        jobs.push(JobInfo {
            job_id,
            user,
            nodes,
            cores,
            gpus,
            memory_bytes: mem_bytes,
        });
    }

    // Compute totals
    let total_cores: u64 = jobs.iter().map(|j| j.cores).sum();
    let total_gpus: u64 = jobs.iter().map(|j| j.gpus).sum();
    let total_mem: u64 = jobs.iter().map(|j| j.memory_bytes).sum();
    let total_nodes: u64 = jobs.iter().map(|j| j.nodes).sum();

    // JSON output
    if output_mode.is_json() {
        let job_jsons: Vec<JobJson> = jobs
            .iter()
            .map(|j| JobJson {
                job_id: j.job_id.clone(),
                user: j.user.clone(),
                nodes: j.nodes,
                cores: j.cores,
                gpus: j.gpus,
                memory_bytes: j.memory_bytes,
            })
            .collect();

        let json = AccountRunningJson {
            module: "account_running",
            version: env!("CARGO_PKG_VERSION"),
            account: args.account.clone(),
            cluster: cluster.map(|c| c.name),
            totals: TotalsJson {
                jobs: jobs.len(),
                cores: total_cores,
                gpus: total_gpus,
                memory_bytes: total_mem,
                nodes: total_nodes,
            },
            jobs: job_jsons,
        };
        println!("{}", serde_json::to_string_pretty(&json)?);
        return Ok(());
    }

    // Human-readable table
    if args.detail {
        print_verbose(
            &args.account,
            &jobs,
            total_nodes,
            total_cores,
            total_gpus,
            total_mem,
        );
    } else {
        print_summary(&args.account, total_cores, total_gpus, total_mem);
    }

    Ok(())
}

/// Parse `scontrol show job` output for a single job.
/// Returns `(user, nodes, cores, gpus, memory_bytes)`.
fn parse_scontrol_job(output: &str) -> (String, u64, u64, u64, u64) {
    let mut user = String::new();
    let mut nodes: u64 = 0;
    let mut cores: u64 = 0;
    let mut gpus: u64 = 0;
    let mut mem_bytes: u64 = 0;

    for line in output.lines() {
        let line = line.trim();

        // UserId=jdoe(12345)
        if let Some(rest) = line.strip_prefix("UserId=").or_else(|| {
            line.split_whitespace()
                .find(|s| s.starts_with("UserId="))
                .and_then(|s| s.strip_prefix("UserId="))
        }) {
            // Extract username before '('
            user = rest.split('(').next().unwrap_or("").to_string();
        }

        // Look for ReqTRES= in the line
        for segment in line.split_whitespace() {
            if let Some(tres_str) = segment.strip_prefix("ReqTRES=") {
                for kv in tres_str.split(',') {
                    if let Some(val) = kv.strip_prefix("cpu=") {
                        cores = val.parse().unwrap_or(0);
                    } else if let Some(val) = kv.strip_prefix("mem=") {
                        mem_bytes = parse_memory(val, 1 << 20).unwrap_or(0);
                    } else if let Some(val) = kv.strip_prefix("node=") {
                        nodes = val.parse().unwrap_or(0);
                    } else if let Some(val) = kv.strip_prefix("gres/gpu=") {
                        gpus = val.parse().unwrap_or(0);
                    }
                }
            }
        }
    }

    (user, nodes, cores, gpus, mem_bytes)
}

/// Format memory in GiB for display (matching legacy format).
fn format_mem_gib(bytes: u64) -> String {
    let gib = bytes as f64 / (1u64 << 30) as f64;
    if gib == gib.round() {
        format!("{:.0} GiB", gib)
    } else {
        format!("{:.3} GiB", gib)
    }
}

fn print_summary(account: &str, cores: u64, gpus: u64, mem: u64) {
    let mut table = Table::new(vec![
        Column {
            header: "ACCOUNT".into(),
            align: Align::Left,
        },
        Column {
            header: "TOTAL_CORES".into(),
            align: Align::Right,
        },
        Column {
            header: "TOTAL_GPUS".into(),
            align: Align::Right,
        },
        Column {
            header: "TOTAL_MEMORY".into(),
            align: Align::Right,
        },
    ]);

    table.add_row(vec![
        account.to_string(),
        cores.to_string(),
        gpus.to_string(),
        format_mem_gib(mem),
    ]);

    print!("{table}");
    println!();
}

fn print_verbose(
    account: &str,
    jobs: &[JobInfo],
    total_nodes: u64,
    total_cores: u64,
    total_gpus: u64,
    total_mem: u64,
) {
    let mut table = Table::new(vec![
        Column {
            header: "ACCOUNT".into(),
            align: Align::Left,
        },
        Column {
            header: "USER".into(),
            align: Align::Left,
        },
        Column {
            header: "JOBID".into(),
            align: Align::Left,
        },
        Column {
            header: "NODES".into(),
            align: Align::Right,
        },
        Column {
            header: "CORES".into(),
            align: Align::Right,
        },
        Column {
            header: "GPUS".into(),
            align: Align::Right,
        },
        Column {
            header: "MEMORY".into(),
            align: Align::Right,
        },
    ]);

    for job in jobs {
        table.add_row(vec![
            account.to_string(),
            job.user.clone(),
            job.job_id.clone(),
            job.nodes.to_string(),
            job.cores.to_string(),
            job.gpus.to_string(),
            format_mem_gib(job.memory_bytes),
        ]);
    }

    table.set_totals(vec![
        "Total:".to_string(),
        String::new(),
        String::new(),
        total_nodes.to_string(),
        total_cores.to_string(),
        total_gpus.to_string(),
        format_mem_gib(total_mem),
    ]);

    print!("{table}");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scontrol_job_basic() {
        let output = r#"
   JobId=12345678 JobName=sim
   UserId=jdoe(12345) GroupId=arc-ts(67890) MCS_label=N/A
   Priority=4294901660 Nice=0 Account=arc-ts QOS=normal
   ReqTRES=cpu=16,mem=64G,node=2,gres/gpu=1
   AllocTRES=cpu=16,mem=64G,node=2
"#;
        let (user, nodes, cores, gpus, mem) = parse_scontrol_job(output);
        assert_eq!(user, "jdoe");
        assert_eq!(nodes, 2);
        assert_eq!(cores, 16);
        assert_eq!(gpus, 1);
        assert_eq!(mem, 64 * (1 << 30));
    }

    #[test]
    fn parse_scontrol_job_no_gpu() {
        let output = r#"
   UserId=jdoe(99999) GroupId=staff(100)
   ReqTRES=cpu=4,mem=8G,node=1
"#;
        let (user, nodes, cores, gpus, mem) = parse_scontrol_job(output);
        assert_eq!(user, "jdoe");
        assert_eq!(nodes, 1);
        assert_eq!(cores, 4);
        assert_eq!(gpus, 0);
        assert_eq!(mem, 8 * (1 << 30));
    }

    #[test]
    fn format_mem_gib_display() {
        assert_eq!(format_mem_gib(10 * (1 << 30)), "10 GiB");
        assert_eq!(format_mem_gib(512 * (1 << 20)), "0.500 GiB");
    }
}
