use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use colored::Colorize;
use serde::Serialize;
use serde_json::Value;

use crate::common::{
    DIVIDER, OutputMode, SpinnerGroup, SpinnerKind, color_dim, color_error, color_info,
    color_job_state, color_success, color_warning, compute_cost, format_dollars, format_memory,
    resolve_user, slurm_cmd,
};

/// Show detailed job statistics and efficiency.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Job ID. Defaults to user's most recent job.
    pub job_id: Option<String>,

    /// Output full job record as pretty-printed JSON.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Serialize)]
struct JobStatsJson {
    module: &'static str,
    version: &'static str,
    cluster: Option<String>,
    auto_detected: bool,
    job: JobJson,
    efficiency: Option<EfficiencyJson>,
    cost_dollars: Option<f64>,
}

#[derive(Serialize)]
struct JobJson {
    job_id: String,
    array_job_id: Option<String>,
    array_task_id: Option<String>,
    job_name: String,
    user: String,
    group: String,
    account: String,
    partition: String,
    state: String,
    exit_code: String,
    nodes: u32,
    alloc_cpus: u32,
    ntasks: u32,
    req_mem_bytes: u64,
    req_mem_per_cpu: bool,
    submit_line: String,
    working_directory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    script_path: Option<String>,
    submit_time: String,
    start_time: String,
    end_time: String,
    queue_wait_seconds: u64,
    elapsed_seconds: u64,
    elapsed_slurm: String,
    walltime_requested_seconds: u64,
    total_cpu_seconds: u64,
    user_cpu_seconds: u64,
    system_cpu_seconds: u64,
    max_rss_bytes: u64,
    max_disk_read_bytes: u64,
    max_disk_write_bytes: u64,
    billing: f64,
    tres_alloc: String,
    stdout_path: String,
    stderr_path: String,
}

#[derive(Serialize)]
struct EfficiencyJson {
    cpu_percent: f64,
    memory_percent: f64,
    walltime_utilized_percent: f64,
}

#[derive(Serialize)]
struct RawJson {
    module: &'static str,
    version: &'static str,
    raw: bool,
    slurm_record: Value,
}

struct JobData {
    job_id: String,
    array_job_id: Option<String>,
    array_task_id: Option<String>,
    job_name: String,
    user: String,
    group: String,
    account: String,
    partition: String,
    cluster: String,
    state: String,
    exit_code: String,
    nodes: u32,
    alloc_cpus: u32,
    ntasks: u32,
    req_mem_bytes: u64,
    req_mem_per_cpu: bool,
    submit_line: String,
    working_directory: String,
    submit_time: String,
    start_time: String,
    end_time: String,
    queue_wait_secs: u64,
    elapsed_secs: u64,
    total_cpu_secs: u64,
    user_cpu_secs: u64,
    system_cpu_secs: u64,
    max_rss_bytes: u64,
    max_disk_read_bytes: u64,
    max_disk_write_bytes: u64,
    billing: f64,
    tres_alloc: String,
    node_list: String,
    walltime_requested_secs: u64,
    stdout_path: String,
    stderr_path: String,
}

const BILLING_DIVISOR: u64 = 10_000_000;

pub async fn run(args: &Args, output_mode: OutputMode) -> Result<()> {
    let show_spinner = !output_mode.is_json() && !args.raw;
    let mut spinner_group = SpinnerGroup::new();
    let spinner = if show_spinner {
        let sp = spinner_group.add(SpinnerKind::Total, "Querying:");
        sp.set_message("sacct");
        Some(sp.clone())
    } else {
        None
    };

    let auto_detected;
    let job_id = if let Some(ref id) = args.job_id {
        auto_detected = false;
        id.clone()
    } else {
        auto_detected = true;
        auto_detect_last_job()
            .await
            .context("auto-detecting last job ID")?
    };

    // Fetch full job via sacct --json
    let raw_output = slurm_cmd(&["sacct", "--json", "-j", &job_id, "--noconvert"])
        .await
        .map_err(|e| anyhow::anyhow!("sacct failed: {e}"))?;

    if let Some(ref sp) = spinner {
        sp.set_message("done");
    }
    spinner_group.finish();

    let json: Value = serde_json::from_str(&raw_output).context("parsing sacct JSON output")?;

    // --raw mode: output full record
    if args.raw {
        let raw_json = RawJson {
            module: "job_stats",
            version: env!("CARGO_PKG_VERSION"),
            raw: true,
            slurm_record: json,
        };
        println!("{}", serde_json::to_string_pretty(&raw_json).unwrap());
        return Ok(());
    }

    // Parse job record
    let jobs_array = json
        .get("jobs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("no 'jobs' array in sacct output"))?;

    if jobs_array.is_empty() {
        bail!("job {job_id} not found");
    }

    // Take the first (parent) job entry
    let jv = &jobs_array[0];
    let job = parse_job_record(jv).with_context(|| format!("parsing job record for {job_id}"))?;

    if output_mode.is_json() {
        print_json(&job, auto_detected);
    } else {
        print_report(&job, auto_detected);
    }

    Ok(())
}

async fn auto_detect_last_job() -> Result<String> {
    let user = resolve_user(None, None).context("resolving user for auto-detect")?;

    let output = slurm_cmd(&[
        "sacct",
        "-u",
        &user,
        "-n",
        "-X",
        "--noconvert",
        "--format=JobID",
        "-Snow-7days",
    ])
    .await
    .map_err(|e| anyhow::anyhow!("sacct failed: {e}"))?;

    let last_line = output
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("no jobs found in the last 7 days for user '{user}'"))?;

    Ok(last_line.trim().to_string())
}

fn parse_job_record(jv: &Value) -> Result<JobData> {
    let s = |key: &str| -> String {
        jv.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    // State: state.current is an array like ["COMPLETED"]
    let state = jv
        .get("state")
        .and_then(|v| v.get("current"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();

    // Exit code: exit_code.return_code.number, signal.id.number
    let exit_code = jv
        .get("exit_code")
        .map(|v| {
            let rc = v
                .get("return_code")
                .and_then(|r| r.get("number"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            let sig = v
                .get("signal")
                .and_then(|s| s.get("id"))
                .and_then(|i| i.get("number"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            if sig > 0 {
                format!("{rc}:{sig}")
            } else {
                rc.to_string()
            }
        })
        .unwrap_or_else(|| "0".to_string());

    // Allocated CPUs: required.CPUs or from tres.allocated cpu entry
    let alloc_cpus = jv
        .get("required")
        .and_then(|r| r.get("CPUs"))
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| tres_count(jv, "cpu")) as u32;

    // Node count from allocation_nodes; node list from nodes (string)
    let nodes = jv
        .get("allocation_nodes")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as u32;

    // Time fields: direct integers under jv.time
    let time = jv.get("time");
    let submit_epoch = time.and_then(|t| t.get("submission")).and_then(as_epoch);
    let start_epoch = time.and_then(|t| t.get("start")).and_then(as_epoch);
    let end_epoch = time.and_then(|t| t.get("end")).and_then(as_epoch);
    let elapsed_secs = time
        .and_then(|t| t.get("elapsed"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Total CPU time: time.total = {seconds, microseconds}
    let total_cpu_secs = time
        .and_then(|t| t.get("total"))
        .map(|total| {
            let secs = total.get("seconds").and_then(|v| v.as_u64()).unwrap_or(0);
            let usecs = total
                .get("microseconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            secs + (usecs + 500_000) / 1_000_000
        })
        .unwrap_or(0);

    // User CPU time: time.user = {seconds, microseconds}
    let user_cpu_secs = time
        .and_then(|t| t.get("user"))
        .map(|u| {
            let secs = u.get("seconds").and_then(|v| v.as_u64()).unwrap_or(0);
            let usecs = u.get("microseconds").and_then(|v| v.as_u64()).unwrap_or(0);
            secs + (usecs + 500_000) / 1_000_000
        })
        .unwrap_or(0);

    // System CPU time: time.system = {seconds, microseconds}
    let system_cpu_secs = time
        .and_then(|t| t.get("system"))
        .map(|sy| {
            let secs = sy.get("seconds").and_then(|v| v.as_u64()).unwrap_or(0);
            let usecs = sy.get("microseconds").and_then(|v| v.as_u64()).unwrap_or(0);
            secs + (usecs + 500_000) / 1_000_000
        })
        .unwrap_or(0);

    // Queue wait time: start - submit (both epoch seconds)
    let queue_wait_secs = match (submit_epoch, start_epoch) {
        (Some(sub), Some(start)) if start > sub => start - sub,
        _ => 0,
    };

    // Walltime limit: time.limit.number (in MINUTES) → convert to seconds
    let walltime_requested_secs = time
        .and_then(|t| t.get("limit"))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.get("number").and_then(|n| n.as_u64()))
        })
        .unwrap_or(0)
        * 60;

    // Required memory
    let (req_mem_bytes, req_mem_per_cpu) = parse_required_memory(jv);

    // MaxRSS from steps
    let max_rss_bytes = parse_max_rss(jv);

    let (max_disk_read_bytes, max_disk_write_bytes) = parse_disk_io(jv);

    let billing = parse_billing_from_tres(jv);

    // TRES allocation as formatted string
    let tres_alloc = format_tres_alloc(jv);

    // Array job handling
    let array_job_id_val = jv
        .get("array")
        .and_then(|a| a.get("job_id"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let array_task_set = jv
        .get("array")
        .and_then(|a| a.get("task_id"))
        .and_then(|t| t.get("set"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let array_task_num = jv
        .get("array")
        .and_then(|a| a.get("task_id"))
        .and_then(|t| t.get("number"))
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);

    let (array_job_id, array_task_id) = if array_job_id_val != 0 {
        (
            Some(array_job_id_val.to_string()),
            if array_task_set && array_task_num >= 0 {
                Some(array_task_num.to_string())
            } else {
                None
            },
        )
    } else {
        (None, None)
    };

    // Submit line (sbatch command)
    let submit_line = s("submit_line");
    let working_directory = s("working_directory");

    // Stdout/stderr expanded paths
    let stdout_path = s("stdout_expanded");
    let stderr_path = s("stderr_expanded");

    // NTasks: not directly available at job level; default to 1
    let ntasks = 1u32;

    Ok(JobData {
        job_id: jv
            .get("job_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .to_string(),
        array_job_id,
        array_task_id,
        job_name: s("name"),
        user: s("user"),
        group: s("group"),
        account: s("account"),
        partition: s("partition"),
        cluster: s("cluster"),
        state,
        exit_code,
        nodes,
        alloc_cpus,
        ntasks,
        req_mem_bytes,
        req_mem_per_cpu,
        submit_line,
        working_directory,
        submit_time: submit_epoch.map(format_epoch).unwrap_or_default(),
        start_time: start_epoch.map(format_epoch).unwrap_or_default(),
        end_time: end_epoch.map(format_epoch).unwrap_or_default(),
        queue_wait_secs,
        elapsed_secs,
        total_cpu_secs,
        user_cpu_secs,
        system_cpu_secs,
        max_rss_bytes,
        max_disk_read_bytes,
        max_disk_write_bytes,
        billing,
        tres_alloc,
        node_list: s("nodes"),
        walltime_requested_secs,
        stdout_path,
        stderr_path,
    })
}

/// Extract an epoch timestamp from a JSON value (direct integer or nested {number}).
fn as_epoch(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_i64().filter(|&i| i > 0).map(|i| i as u64))
        .or_else(|| v.get("number").and_then(|n| n.as_u64()))
        .filter(|&n| n > 0)
}

fn format_epoch(epoch: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(epoch as i64, 0)
        .single()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| epoch.to_string())
}

/// Format seconds as `HH:MM:SS` or `D-HH:MM:SS` (matching legacy Slurm style).
fn format_duration_hms(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days}-{hours:02}:{mins:02}:{s:02}")
    } else {
        format!("{hours:02}:{mins:02}:{s:02}")
    }
}

/// Color an efficiency percentage: green ≥75%, warning ≥25%, red <25%.
fn color_efficiency(pct: f64) -> String {
    let label = format!("{pct:.2}%");
    if pct >= 75.0 {
        color_success(&label).to_string()
    } else if pct >= 25.0 {
        color_warning(&label).to_string()
    } else {
        color_error(&label).to_string()
    }
}

/// Color exit code: green for 0 or 0:0, red otherwise.
fn color_exit_code(code: &str) -> String {
    if code == "0" || code == "0:0" {
        color_success(code).to_string()
    } else {
        color_error(code).to_string()
    }
}

/// Get the count for a TRES type from the tres.allocated array.
fn tres_count(jv: &Value, tres_type: &str) -> u64 {
    jv.get("tres")
        .and_then(|t| t.get("allocated"))
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|e| e.get("type").and_then(|v| v.as_str()) == Some(tres_type))
        })
        .and_then(|e| e.get("count").and_then(|v| v.as_u64()))
        .unwrap_or(0)
}

/// Format the tres.allocated array as a "key=val,..." string.
fn format_tres_alloc(jv: &Value) -> String {
    if let Some(arr) = jv
        .get("tres")
        .and_then(|t| t.get("allocated"))
        .and_then(|a| a.as_array())
    {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|entry| {
                let t = entry.get("type")?.as_str()?;
                let c = entry.get("count")?.as_i64()?;
                Some(format!("{t}={c}"))
            })
            .collect();
        return parts.join(",");
    }
    jv.get("tres")
        .and_then(|t| t.get("allocated"))
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .to_string()
}

/// Parse required memory from sacct JSON.
///
/// Slurm 25+ stores `required.memory_per_{cpu,node}` as `{number, set, infinite}`
/// objects where `number` is in MB.
fn parse_required_memory(jv: &Value) -> (u64, bool) {
    if let Some(required) = jv.get("required") {
        // memory_per_cpu
        if let Some(obj) = required.get("memory_per_cpu") {
            if obj.get("set").and_then(|v| v.as_bool()).unwrap_or(false) {
                let mb = obj.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
                if mb > 0 {
                    let cpus = required.get("CPUs").and_then(|v| v.as_u64()).unwrap_or(1);
                    return (mb * 1024 * 1024 * cpus, true);
                }
            }
        }
        // memory_per_node
        if let Some(obj) = required.get("memory_per_node") {
            if obj.get("set").and_then(|v| v.as_bool()).unwrap_or(false) {
                let mb = obj.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
                if mb > 0 {
                    let nodes = jv
                        .get("allocation_nodes")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(1);
                    return (mb * 1024 * 1024 * nodes, false);
                }
            }
        }
    }

    // Fallback: parse from TRES allocated string
    let tres_str = jv
        .get("tres")
        .and_then(|v| {
            v.get("requested")
                .and_then(|r| r.as_str())
                .or_else(|| v.get("allocated").and_then(|a| a.as_str()))
        })
        .unwrap_or("");

    let mem_bytes = parse_tres_mem(tres_str);
    (mem_bytes, false)
}

/// Parse mem= value from TRES string.
fn parse_tres_mem(tres: &str) -> u64 {
    for part in tres.split(',') {
        if let Some(val) = part.strip_prefix("mem=") {
            return parse_mem_str(val);
        }
    }
    0
}

/// Parse a memory string like "180G" or "4096M" to bytes.
fn parse_mem_str(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }

    let (num_str, unit) = if s.ends_with(|c: char| c.is_ascii_alphabetic()) {
        let boundary = s.len() - 1;
        (&s[..boundary], s.chars().last().unwrap())
    } else {
        (s, 'M') // default MB
    };

    let val: f64 = num_str.parse().unwrap_or(0.0);
    match unit.to_ascii_uppercase() {
        'T' => (val * (1u64 << 40) as f64) as u64,
        'G' => (val * (1u64 << 30) as f64) as u64,
        'M' => (val * (1u64 << 20) as f64) as u64,
        'K' => (val * (1u64 << 10) as f64) as u64,
        _ => (val * (1u64 << 20) as f64) as u64,
    }
}

/// Parse max RSS from job steps in sacct JSON.
///
/// In Slurm 25+ the actual memory usage is stored in each step's
/// `tres.requested.max` array (type=="mem") with count in bytes.
fn parse_max_rss(jv: &Value) -> u64 {
    let mut max_rss: u64 = 0;

    if let Some(steps) = jv.get("steps").and_then(|v| v.as_array()) {
        for step in steps {
            // step.tres.requested.max[] where type=="mem"
            if let Some(arr) = step
                .get("tres")
                .and_then(|t| t.get("requested"))
                .and_then(|r| r.get("max"))
                .and_then(|m| m.as_array())
            {
                for entry in arr {
                    if entry.get("type").and_then(|v| v.as_str()) == Some("mem") {
                        let count = entry.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                        if count > max_rss {
                            max_rss = count;
                        }
                    }
                }
            }
        }
    }

    max_rss
}

fn parse_disk_io(jv: &Value) -> (u64, u64) {
    let mut max_read: u64 = 0;
    let mut max_write: u64 = 0;

    if let Some(steps) = jv.get("steps").and_then(|v| v.as_array()) {
        for step in steps {
            if let Some(tres) = step.get("tres") {
                if let Some(arr) = tres
                    .get("requested")
                    .and_then(|r| r.get("max"))
                    .and_then(|m| m.as_array())
                {
                    for entry in arr {
                        if entry.get("type").and_then(|v| v.as_str()) == Some("fs")
                            && entry.get("name").and_then(|v| v.as_str()) == Some("disk")
                        {
                            let count = entry.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                            if count > max_read {
                                max_read = count;
                            }
                        }
                    }
                }
                if let Some(arr) = tres
                    .get("consumed")
                    .and_then(|r| r.get("max"))
                    .and_then(|m| m.as_array())
                {
                    for entry in arr {
                        if entry.get("type").and_then(|v| v.as_str()) == Some("fs")
                            && entry.get("name").and_then(|v| v.as_str()) == Some("disk")
                        {
                            let count = entry.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                            if count > max_write {
                                max_write = count;
                            }
                        }
                    }
                }
            }
        }
    }

    (max_read, max_write)
}

/// Parse billing value from TRES in sacct JSON.
fn parse_billing_from_tres(jv: &Value) -> f64 {
    // Try tres -> allocated -> billing
    if let Some(tres) = jv.get("tres") {
        if let Some(allocated) = tres.get("allocated") {
            if let Some(arr) = allocated.as_array() {
                for entry in arr {
                    let name = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if name == "billing" {
                        return entry.get("count").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    }
                }
            }
            // TRES as string
            if let Some(s) = allocated.as_str() {
                for part in s.split(',') {
                    if let Some(val) = part.strip_prefix("billing=") {
                        return val.parse().unwrap_or(0.0);
                    }
                }
            }
        }
    }
    0.0
}

fn print_report(job: &JobData, auto_detected: bool) {
    if auto_detected {
        eprintln!(
            "{}",
            color_dim(&format!("(auto-detected job {})", job.job_id))
        );
    }

    println!(
        "{}{}{}{}{}{}",
        "Job summary for JobID ".bold(),
        color_info(&job.job_id).bold(),
        " on the ".bold(),
        color_info(&job.cluster).bold(),
        " cluster for ".bold(),
        color_info(&job.user).bold(),
    );
    println!("{}", format!("Job name: {}", job.job_name).bold());
    println!("{DIVIDER}");

    // Submit command
    if !job.submit_line.is_empty() {
        println!("{:<20} {}", "Submit command:", job.submit_line);
    }
    if !job.working_directory.is_empty() {
        println!("{:<20} {}", "Working directory:", job.working_directory);
    }
    if let Some(path) = resolve_script_path(&job.submit_line, &job.working_directory) {
        println!("{:<20} {}", "Script path:", path);
    }

    println!();

    // Times
    if !job.submit_time.is_empty() {
        println!(
            "{:<20} {}",
            "Job submit time:",
            format_display_time(&job.submit_time)
        );
    }

    let is_pending = job.state == "PENDING";
    let is_terminal = matches!(
        job.state.as_str(),
        "COMPLETED" | "CANCELLED" | "FAILED" | "OUT_OF_MEMORY" | "TIMEOUT"
    );

    if !is_pending && job.queue_wait_secs > 0 {
        println!(
            "{:<20} {}",
            "Queue wait time:",
            format_duration_hms(job.queue_wait_secs)
        );
    }
    if !is_pending && !job.start_time.is_empty() {
        println!(
            "{:<20} {}",
            "Job start time:",
            format_display_time(&job.start_time)
        );
    }
    if is_terminal && !job.end_time.is_empty() {
        println!(
            "{:<20} {}",
            "Job end time:",
            format_display_time(&job.end_time)
        );
    }
    if !is_pending && job.elapsed_secs > 0 {
        println!(
            "{:<20} {}",
            "Job running time:",
            format_duration_hms(job.elapsed_secs)
        );
    }
    if is_terminal && job.walltime_requested_secs > 0 {
        println!(
            "{:<20} {}",
            "Walltime requested:",
            format_duration_hms(job.walltime_requested_secs)
        );
    }

    // State + exit code + stdout/stderr
    if is_pending || job.state == "RUNNING" {
        println!("\n{:<20} {}", "State:", color_job_state(&job.state));
    } else {
        println!("\n{:<20} {}", "State:", color_job_state(&job.state));
        println!("{:<20} {}", "Exit code:", color_exit_code(&job.exit_code));
    }
    if !job.stdout_path.is_empty() {
        println!("{:<20} {}", "Stdout:", job.stdout_path);
    }
    if !job.stderr_path.is_empty() {
        println!("{:<20} {}", "Stderr:", job.stderr_path);
    }

    // Account + partition + nodes/cores
    println!("\n{:<20} {}", "Account:", job.account);
    println!("{:<20} {}", "Partition:", job.partition);
    if job.alloc_cpus == 1 {
        println!("{:<20} {}", "Cores:", job.alloc_cpus);
        println!("{:<20} {}", "On node:", job.node_list);
    } else {
        println!("{:<20} {}", "On nodes:", job.node_list);
        let nodes = if job.nodes == 0 { 1 } else { job.nodes };
        let cores_per_node = job.alloc_cpus / nodes;
        if is_pending {
            println!(
                "{:<20} ({} nodes with {} cores per node requested)",
                " ", nodes, cores_per_node
            );
        } else {
            println!(
                "{:<20} ({} nodes with {} cores per node)",
                " ", nodes, cores_per_node
            );
        }
    }

    // Efficiency metrics (only for terminal states)
    if is_terminal {
        let core_walltime = job.elapsed_secs as f64 * job.alloc_cpus as f64;
        let cpu_eff = if core_walltime > 0.0 {
            job.total_cpu_secs as f64 / core_walltime * 100.0
        } else {
            0.0
        };

        println!(
            "\n{:<20} {}",
            "CPU Utilized:",
            format_duration_hms(job.total_cpu_secs)
        );
        if job.user_cpu_secs > 0 || job.system_cpu_secs > 0 {
            println!(
                "{:<20} {} user, {} system",
                " ",
                format_duration_hms(job.user_cpu_secs),
                format_duration_hms(job.system_cpu_secs)
            );
        }
        println!(
            "{:<20} {} of {} total CPU time (cores * walltime)",
            "CPU Efficiency:",
            color_efficiency(cpu_eff),
            format_duration_hms(core_walltime as u64)
        );

        // Memory
        let ntasks = if job.ntasks == 0 { 1 } else { job.ntasks };
        let mem_used = job.max_rss_bytes;

        if ntasks == 1 {
            println!("\n{:<20} {}", "Memory Utilized:", format_memory(mem_used));
        } else {
            println!(
                "\n{:<20} {} (estimated maximum)",
                "Memory Utilized:",
                format_memory(mem_used)
            );
        }

        let mem_eff = if job.req_mem_bytes > 0 {
            mem_used as f64 / job.req_mem_bytes as f64 * 100.0
        } else {
            0.0
        };

        if ntasks == 1 {
            println!(
                "{:<20} {} of {}",
                "Memory Efficiency:",
                color_efficiency(mem_eff),
                format_memory(job.req_mem_bytes)
            );
        } else if job.req_mem_per_cpu {
            let per_cpu = if job.alloc_cpus > 0 {
                job.req_mem_bytes / job.alloc_cpus as u64
            } else {
                job.req_mem_bytes
            };
            println!(
                "{:<20} {} of {} ({}/core)",
                "Memory Efficiency:",
                color_efficiency(mem_eff),
                format_memory(job.req_mem_bytes),
                format_memory(per_cpu)
            );
        } else {
            let nodes = if job.nodes == 0 { 1 } else { job.nodes };
            let per_node = job.req_mem_bytes / nodes as u64;
            println!(
                "{:<20} {} of {} ({}/node)",
                "Memory Efficiency:",
                color_efficiency(mem_eff),
                format_memory(job.req_mem_bytes),
                format_memory(per_node)
            );
        }

        if job.max_disk_read_bytes > 0 || job.max_disk_write_bytes > 0 {
            println!();
            if job.max_disk_read_bytes > 0 {
                println!(
                    "{:<20} {}",
                    "Max Disk Read:",
                    format_memory(job.max_disk_read_bytes)
                );
            }
            if job.max_disk_write_bytes > 0 {
                println!(
                    "{:<20} {}",
                    "Max Disk Write:",
                    format_memory(job.max_disk_write_bytes)
                );
            }
        }

        // Cost
        if job.cluster != "lighthouse" && job.billing > 0.0 {
            let cost = compute_cost(job.billing, job.elapsed_secs as f64 / 60.0, BILLING_DIVISOR);
            println!("{:<20} {}", "Cost:", format_dollars(cost).bold());
        }

        if matches!(job.state.as_str(), "COMPLETED" | "TIMEOUT") {
            let mut tips: Vec<String> = Vec::new();

            if job.req_mem_bytes > 0 && (mem_used as f64) < job.req_mem_bytes as f64 * 0.5 {
                tips.push(format!(
                    "TIP: Job used {} of {} memory. Consider requesting less.",
                    format_memory(mem_used),
                    format_memory(job.req_mem_bytes)
                ));
            }

            if cpu_eff < 50.0 && job.alloc_cpus > 1 {
                if job.job_name.starts_with("ondemand/") {
                    tips.push(
                        "TIP: Low CPU efficiency. Remember to delete your session when done to avoid idle charges.".to_string()
                    );
                } else {
                    tips.push(format!(
                        "TIP: CPU efficiency was {cpu_eff:.1}%. Consider requesting fewer cores."
                    ));
                }
            }

            if job.state != "TIMEOUT"
                && job.walltime_requested_secs > 0
                && job.elapsed_secs < job.walltime_requested_secs / 2
            {
                tips.push(format!(
                    "TIP: Job ran {} of {} walltime. Consider requesting less.",
                    format_duration_hms(job.elapsed_secs),
                    format_duration_hms(job.walltime_requested_secs)
                ));
            }

            if !tips.is_empty() {
                println!();
                for tip in &tips {
                    println!("{}", color_dim(tip));
                }
            }
        }
    } else {
        // Pending/running: show requests
        println!(
            "\n{:<20} {}",
            "Walltime request:",
            format_duration_hms(job.walltime_requested_secs)
        );
        println!("{:<20} {}", "Processors request:", job.alloc_cpus);
        println!(
            "{:<20} {}",
            "Memory request:",
            format_memory(job.req_mem_bytes)
        );
        if job.cluster != "lighthouse" && job.billing > 0.0 {
            let max_cost = compute_cost(
                job.billing,
                job.walltime_requested_secs as f64 / 60.0,
                BILLING_DIVISOR,
            );
            println!(
                "{:<20} {}",
                "Maximum job charge:",
                format_dollars(max_cost).bold()
            );
        }
    }

    println!("{DIVIDER}");
}

/// Format an RFC3339 or epoch timestamp for display (MM/DD/YYYY HH:MM:SS).
fn format_display_time(s: &str) -> String {
    // Try parsing as RFC3339
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.format("%m/%d/%Y %H:%M:%S").to_string();
    }
    // Try parsing as epoch
    if let Ok(epoch) = s.parse::<i64>() {
        use chrono::TimeZone;
        if let Some(dt) = chrono::Local.timestamp_opt(epoch, 0).single() {
            return dt.format("%m/%d/%Y %H:%M:%S").to_string();
        }
    }
    s.to_string()
}

fn print_json(job: &JobData, auto_detected: bool) {
    let cost = if job.cluster != "lighthouse" && job.billing > 0.0 {
        let is_terminal = matches!(
            job.state.as_str(),
            "COMPLETED" | "CANCELLED" | "FAILED" | "OUT_OF_MEMORY" | "TIMEOUT"
        );
        let minutes = if is_terminal {
            job.elapsed_secs as f64 / 60.0
        } else {
            job.walltime_requested_secs as f64 / 60.0
        };
        Some(compute_cost(job.billing, minutes, BILLING_DIVISOR))
    } else {
        None
    };

    let is_terminal = matches!(
        job.state.as_str(),
        "COMPLETED" | "CANCELLED" | "FAILED" | "OUT_OF_MEMORY" | "TIMEOUT"
    );

    let efficiency = if is_terminal {
        let core_walltime = job.elapsed_secs as f64 * job.alloc_cpus as f64;
        let cpu_pct = if core_walltime > 0.0 {
            job.total_cpu_secs as f64 / core_walltime * 100.0
        } else {
            0.0
        };
        let mem_pct = if job.req_mem_bytes > 0 {
            job.max_rss_bytes as f64 / job.req_mem_bytes as f64 * 100.0
        } else {
            0.0
        };
        Some(EfficiencyJson {
            cpu_percent: (cpu_pct * 10.0).round() / 10.0,
            memory_percent: (mem_pct * 10.0).round() / 10.0,
            walltime_utilized_percent: if job.walltime_requested_secs > 0 {
                ((job.elapsed_secs as f64 / job.walltime_requested_secs as f64) * 1000.0).round()
                    / 10.0
            } else {
                0.0
            },
        })
    } else {
        None
    };

    let json = JobStatsJson {
        module: "job_stats",
        version: env!("CARGO_PKG_VERSION"),
        cluster: Some(job.cluster.clone()),
        auto_detected,
        job: JobJson {
            job_id: job.job_id.clone(),
            array_job_id: job.array_job_id.clone(),
            array_task_id: job.array_task_id.clone(),
            job_name: job.job_name.clone(),
            user: job.user.clone(),
            group: job.group.clone(),
            account: job.account.clone(),
            partition: job.partition.clone(),
            state: job.state.clone(),
            exit_code: job.exit_code.clone(),
            nodes: job.nodes,
            alloc_cpus: job.alloc_cpus,
            ntasks: job.ntasks,
            req_mem_bytes: job.req_mem_bytes,
            req_mem_per_cpu: job.req_mem_per_cpu,
            submit_line: job.submit_line.clone(),
            working_directory: job.working_directory.clone(),
            script_path: resolve_script_path(&job.submit_line, &job.working_directory),
            submit_time: job.submit_time.clone(),
            start_time: job.start_time.clone(),
            end_time: job.end_time.clone(),
            queue_wait_seconds: job.queue_wait_secs,
            elapsed_seconds: job.elapsed_secs,
            elapsed_slurm: format_duration_hms(job.elapsed_secs),
            walltime_requested_seconds: job.walltime_requested_secs,
            total_cpu_seconds: job.total_cpu_secs,
            user_cpu_seconds: job.user_cpu_secs,
            system_cpu_seconds: job.system_cpu_secs,
            max_rss_bytes: job.max_rss_bytes,
            max_disk_read_bytes: job.max_disk_read_bytes,
            max_disk_write_bytes: job.max_disk_write_bytes,
            billing: job.billing,
            tres_alloc: job.tres_alloc.clone(),
            stdout_path: job.stdout_path.clone(),
            stderr_path: job.stderr_path.clone(),
        },
        efficiency,
        cost_dollars: cost,
    };

    println!("{}", serde_json::to_string_pretty(&json).unwrap());
}

/// Resolve the full script path from the sbatch submit line and working directory.
fn resolve_script_path(submit_line: &str, work_dir: &str) -> Option<String> {
    if submit_line.is_empty() || work_dir.is_empty() {
        return None;
    }

    let mut args = submit_line.split_whitespace();
    args.next(); // skip "sbatch" / "srun" / etc.

    while let Some(arg) = args.next() {
        if arg == "--" {
            return args.next().map(|script| resolve_path(work_dir, script));
        }
        if arg == "--wrap" || arg.starts_with("--wrap=") {
            return None;
        }
        if arg.starts_with('-') {
            if !arg.contains('=') && is_sbatch_value_flag(arg) {
                args.next(); // skip the value
            }
            continue;
        }
        return Some(resolve_path(work_dir, arg));
    }
    None
}

fn resolve_path(work_dir: &str, script: &str) -> String {
    if script.starts_with('/') {
        script.to_string()
    } else {
        let script = script.strip_prefix("./").unwrap_or(script);
        format!("{}/{}", work_dir.trim_end_matches('/'), script)
    }
}

fn is_sbatch_value_flag(flag: &str) -> bool {
    const VALUE_FLAGS: &[&str] = &[
        "-A",
        "--account",
        "-p",
        "--partition",
        "-J",
        "--job-name",
        "-o",
        "--output",
        "-e",
        "--error",
        "-t",
        "--time",
        "-n",
        "--ntasks",
        "-c",
        "--cpus-per-task",
        "-N",
        "--nodes",
        "--mem",
        "--mem-per-cpu",
        "--mem-per-gpu",
        "--gres",
        "--gpus",
        "--gpus-per-node",
        "--gpus-per-task",
        "-w",
        "--nodelist",
        "-x",
        "--exclude",
        "--qos",
        "--constraint",
        "-d",
        "--dependency",
        "--mail-type",
        "--mail-user",
        "--export",
        "--array",
        "--begin",
        "--deadline",
        "-D",
        "--chdir",
        "-M",
        "--clusters",
    ];
    VALUE_FLAGS.contains(&flag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tres_mem() {
        assert_eq!(
            parse_tres_mem("billing=36,cpu=36,mem=180G,node=1"),
            180 * (1 << 30)
        );
        assert_eq!(parse_tres_mem("cpu=4,mem=4096M"), 4096 * (1 << 20));
        assert_eq!(parse_tres_mem("cpu=1,node=1"), 0);
    }

    #[test]
    fn test_parse_mem_str() {
        assert_eq!(parse_mem_str("180G"), 180 * (1 << 30));
        assert_eq!(parse_mem_str("4096M"), 4096 * (1 << 20));
        assert_eq!(parse_mem_str("1T"), 1u64 << 40);
        assert_eq!(parse_mem_str(""), 0);
    }

    #[test]
    fn test_format_display_time() {
        let ts = "2026-04-13T10:00:00-04:00";
        let display = format_display_time(ts);
        assert!(display.contains("2026"), "got: {display}");
        assert!(display.contains("10:00:00") || display.contains("14:00:00"));
    }

    #[test]
    fn test_cpu_efficiency_calculation() {
        // 36 CPUs, 5100s wall, 172800s CPU time
        let core_walltime = 5100.0_f64 * 36.0;
        let cpu_eff = 172800.0 / core_walltime * 100.0;
        assert!((cpu_eff - 94.1).abs() < 0.2);
    }

    #[test]
    fn test_memory_efficiency_calculation() {
        // max_rss = 128 GiB, req = 180 GiB
        let max_rss = 128u64 * (1 << 30);
        let req_mem = 180u64 * (1 << 30);
        let mem_eff = max_rss as f64 / req_mem as f64 * 100.0;
        assert!((mem_eff - 71.1).abs() < 0.1);
    }

    #[test]
    fn test_cost_calculation() {
        // billing=36, 5100s elapsed = 85 min. Cost = 36 * 85 / 10_000_000
        let cost = compute_cost(36.0, 85.0, BILLING_DIVISOR);
        assert!((cost - 0.000306).abs() < 1e-6);
    }

    #[test]
    fn test_parse_billing_from_tres_string() {
        let json = serde_json::json!({
            "tres": {
                "allocated": "billing=36,cpu=36,mem=180G,node=1"
            }
        });
        let billing = parse_billing_from_tres(&json);
        assert!((billing - 36.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_billing_from_tres_array() {
        let json = serde_json::json!({
            "tres": {
                "allocated": [
                    {"type": "cpu", "count": 36},
                    {"type": "billing", "count": 36},
                    {"type": "mem", "count": 193273528320_u64}
                ]
            }
        });
        let billing = parse_billing_from_tres(&json);
        assert!((billing - 36.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_format_duration_hms() {
        assert_eq!(format_duration_hms(12), "00:00:12");
        assert_eq!(format_duration_hms(3661), "01:01:01");
        assert_eq!(format_duration_hms(86400 + 3600), "1-01:00:00");
        assert_eq!(format_duration_hms(0), "00:00:00");
    }

    #[test]
    fn test_parse_job_record_slurm25() {
        let jv = serde_json::json!({
            "job_id": 10000001,
            "name": "test_sim",
            "user": "jdoe",
            "group": "jdoe",
            "account": "example_class",
            "partition": "standard",
            "cluster": "greatlakes",
            "nodes": "node001",
            "allocation_nodes": 1,
            "submit_line": "sbatch scripts/run_sim.sh",
            "stdout_expanded": "/home/jdoe/logs/sim_10000001.out",
            "stderr_expanded": "/home/jdoe/logs/sim_10000001.err",
            "state": { "current": ["COMPLETED"], "reason": "None" },
            "exit_code": {
                "return_code": { "number": 0, "set": true, "infinite": false },
                "signal": { "id": { "number": 0, "set": false, "infinite": false }, "name": "" },
                "status": ["SUCCESS"]
            },
            "required": {
                "CPUs": 2,
                "memory_per_cpu": { "number": 0, "set": false, "infinite": false },
                "memory_per_node": { "number": 600, "set": true, "infinite": false }
            },
            "array": {
                "job_id": 0,
                "task_id": { "number": 0, "set": false, "infinite": false }
            },
            "time": {
                "submission": 1700000000_u64,
                "start": 1700000001_u64,
                "end": 1700000013_u64,
                "elapsed": 12,
                "limit": { "number": 1, "set": true, "infinite": false },
                "total": { "seconds": 11, "microseconds": 578239 },
                "system": { "seconds": 2, "microseconds": 225766 },
                "user": { "seconds": 9, "microseconds": 352473 }
            },
            "tres": {
                "allocated": [
                    { "type": "cpu", "id": 1, "name": "", "count": 2 },
                    { "type": "mem", "id": 2, "name": "", "count": 600 },
                    { "type": "node", "id": 4, "name": "", "count": 1 },
                    { "type": "billing", "id": 5, "name": "", "count": 5009 }
                ]
            },
            "steps": [{
                "step": { "name": "batch" },
                "tres": {
                    "requested": {
                        "max": [
                            { "type": "mem", "id": 2, "name": "", "count": 278548480_u64, "node": "node001", "task": 0 }
                        ]
                    }
                }
            }]
        });

        let job = parse_job_record(&jv).unwrap();
        assert_eq!(job.state, "COMPLETED");
        assert_eq!(job.exit_code, "0");
        assert_eq!(job.alloc_cpus, 2);
        assert_eq!(job.nodes, 1);
        assert_eq!(job.elapsed_secs, 12);
        assert_eq!(job.total_cpu_secs, 12); // 11s + 578239µs rounds to 12
        assert_eq!(job.user_cpu_secs, 9); // 9s + 352473µs rounds to 9
        assert_eq!(job.system_cpu_secs, 2); // 2s + 225766µs rounds to 2
        assert_eq!(job.walltime_requested_secs, 60); // 1 minute
        assert_eq!(job.req_mem_bytes, 600 * 1024 * 1024); // 600 MiB
        assert!(!job.req_mem_per_cpu);
        assert_eq!(job.max_rss_bytes, 278548480);
        assert!((job.billing - 5009.0).abs() < f64::EPSILON);
        assert_eq!(job.node_list, "node001");
        assert_eq!(job.account, "example_class");
        assert_eq!(job.partition, "standard");
        assert_eq!(job.submit_line, "sbatch scripts/run_sim.sh");
        assert_eq!(job.stdout_path, "/home/jdoe/logs/sim_10000001.out");
        assert_eq!(job.stderr_path, "/home/jdoe/logs/sim_10000001.err");
        assert_eq!(job.queue_wait_secs, 1); // 1700000001 - 1700000000

        // Verify efficiency matches legacy output
        let core_walltime = job.elapsed_secs as f64 * job.alloc_cpus as f64;
        let cpu_eff = job.total_cpu_secs as f64 / core_walltime * 100.0;
        assert!((cpu_eff - 50.0).abs() < 0.1);

        let mem_eff = job.max_rss_bytes as f64 / job.req_mem_bytes as f64 * 100.0;
        assert!((mem_eff - 44.27).abs() < 0.01);
    }

    #[test]
    fn test_queue_wait_zero_when_immediate() {
        let jv = serde_json::json!({
            "job_id": 100,
            "name": "test",
            "user": "user",
            "group": "grp",
            "account": "acct",
            "partition": "standard",
            "cluster": "greatlakes",
            "nodes": "n1",
            "allocation_nodes": 1,
            "submit_line": "",
            "stdout_expanded": "",
            "stderr_expanded": "",
            "state": { "current": ["COMPLETED"], "reason": "None" },
            "exit_code": {
                "return_code": { "number": 0, "set": true, "infinite": false },
                "signal": { "id": { "number": 0, "set": false, "infinite": false }, "name": "" },
                "status": ["SUCCESS"]
            },
            "required": {
                "CPUs": 1,
                "memory_per_cpu": { "number": 0, "set": false, "infinite": false },
                "memory_per_node": { "number": 100, "set": true, "infinite": false }
            },
            "array": {
                "job_id": 0,
                "task_id": { "number": 0, "set": false, "infinite": false }
            },
            "time": {
                "submission": 1000_u64,
                "start": 1000_u64,
                "end": 1010_u64,
                "elapsed": 10,
                "limit": { "number": 5, "set": true, "infinite": false },
                "total": { "seconds": 8, "microseconds": 0 },
                "system": { "seconds": 1, "microseconds": 0 },
                "user": { "seconds": 7, "microseconds": 0 }
            },
            "tres": { "allocated": [] },
            "steps": []
        });

        let job = parse_job_record(&jv).unwrap();
        assert_eq!(job.queue_wait_secs, 0);
        assert_eq!(job.user_cpu_secs, 7);
        assert_eq!(job.system_cpu_secs, 1);
        assert_eq!(job.total_cpu_secs, 8);
    }

    #[test]
    fn test_exit_code_with_signal() {
        let jv = serde_json::json!({
            "job_id": 200,
            "name": "killed",
            "user": "u",
            "group": "g",
            "account": "a",
            "partition": "p",
            "cluster": "c",
            "nodes": "n1",
            "allocation_nodes": 1,
            "submit_line": "",
            "stdout_expanded": "",
            "stderr_expanded": "",
            "state": { "current": ["FAILED"], "reason": "None" },
            "exit_code": {
                "return_code": { "number": 0, "set": true, "infinite": false },
                "signal": { "id": { "number": 9, "set": true, "infinite": false }, "name": "SIGKILL" },
                "status": ["SIGNALED"]
            },
            "required": {
                "CPUs": 1,
                "memory_per_cpu": { "number": 0, "set": false, "infinite": false },
                "memory_per_node": { "number": 0, "set": false, "infinite": false }
            },
            "array": {
                "job_id": 0,
                "task_id": { "number": 0, "set": false, "infinite": false }
            },
            "time": {
                "submission": 1000_u64,
                "start": 1001_u64,
                "end": 1010_u64,
                "elapsed": 9,
                "limit": { "number": 1, "set": true, "infinite": false },
                "total": { "seconds": 0, "microseconds": 0 },
                "system": { "seconds": 0, "microseconds": 0 },
                "user": { "seconds": 0, "microseconds": 0 }
            },
            "tres": { "allocated": [] },
            "steps": []
        });

        let job = parse_job_record(&jv).unwrap();
        assert_eq!(job.exit_code, "0:9");
        assert_eq!(job.state, "FAILED");
    }

    #[test]
    fn test_resolve_script_path() {
        assert_eq!(
            resolve_script_path("sbatch train.sh", "/home/user"),
            Some("/home/user/train.sh".to_string())
        );
        assert_eq!(
            resolve_script_path("sbatch /opt/train.sh", "/home/user"),
            Some("/opt/train.sh".to_string())
        );
        assert_eq!(
            resolve_script_path("sbatch --account=myacct train.sh", "/home/user"),
            Some("/home/user/train.sh".to_string())
        );
        assert_eq!(
            resolve_script_path("sbatch -A myacct -p gpu train.sh", "/home/user"),
            Some("/home/user/train.sh".to_string())
        );
        assert_eq!(
            resolve_script_path("sbatch --wrap=\"echo hello\"", "/home/user"),
            None
        );
        assert_eq!(
            resolve_script_path("sbatch -- train.sh", "/home/user"),
            Some("/home/user/train.sh".to_string())
        );
        assert_eq!(resolve_script_path("", "/home/user"), None);
        assert_eq!(resolve_script_path("sbatch train.sh", ""), None);
        assert_eq!(
            resolve_script_path("sbatch scripts/train.sh", "/home/user/projects"),
            Some("/home/user/projects/scripts/train.sh".to_string())
        );
        assert_eq!(
            resolve_script_path("sbatch ./Sbatch/run.sbatch", "/home/user/project"),
            Some("/home/user/project/Sbatch/run.sbatch".to_string())
        );
        assert_eq!(
            resolve_script_path("sbatch ./run.sh", "/home/user"),
            Some("/home/user/run.sh".to_string())
        );
        assert_eq!(
            resolve_script_path("sbatch -D /tmp/workdir --parsable -M armis2", "/home/user"),
            None
        );
        assert_eq!(
            resolve_script_path("sbatch -D /tmp/workdir run.sh", "/home/user"),
            Some("/home/user/run.sh".to_string())
        );
    }

    #[test]
    fn test_parse_disk_io_with_steps() {
        // batch + extern steps with disk I/O
        let jv = serde_json::json!({
            "steps": [
                {
                    "step": { "name": "batch" },
                    "tres": {
                        "requested": {
                            "max": [
                                { "type": "fs", "name": "disk", "count": 1048576_u64 }
                            ]
                        },
                        "consumed": {
                            "max": [
                                { "type": "fs", "name": "disk", "count": 524288_u64 }
                            ]
                        }
                    }
                },
                {
                    "step": { "name": "extern" },
                    "tres": {
                        "requested": {
                            "max": [
                                { "type": "fs", "name": "disk", "count": 0 }
                            ]
                        },
                        "consumed": {
                            "max": [
                                { "type": "fs", "name": "disk", "count": 0 }
                            ]
                        }
                    }
                }
            ]
        });
        let (read, write) = parse_disk_io(&jv);
        assert_eq!(read, 1048576);
        assert_eq!(write, 524288);
    }

    #[test]
    fn test_parse_disk_io_no_steps() {
        let jv = serde_json::json!({ "steps": [] });
        let (read, write) = parse_disk_io(&jv);
        assert_eq!(read, 0);
        assert_eq!(write, 0);
    }

    #[test]
    fn test_parse_disk_io_missing_steps() {
        let jv = serde_json::json!({});
        let (read, write) = parse_disk_io(&jv);
        assert_eq!(read, 0);
        assert_eq!(write, 0);
    }

    #[test]
    fn test_parse_job_record_timeout_state() {
        // TIMEOUT job with SIGTERM exit
        let jv = serde_json::json!({
            "job_id": 47421353,
            "name": "ondemand/sys/dashboard/batch_connect/sys/ood_rstudio",
            "user": "testuser",
            "group": "testgrp",
            "account": "testacct",
            "partition": "standard",
            "cluster": "greatlakes",
            "nodes": "gl3100",
            "allocation_nodes": 1,
            "submit_line": "sbatch --parsable",
            "working_directory": "/home/testuser",
            "stdout_expanded": "/home/testuser/ondemand/output.log",
            "stderr_expanded": "",
            "state": { "current": ["TIMEOUT"], "reason": "TimeLimit" },
            "exit_code": {
                "return_code": { "number": 0, "set": true, "infinite": false },
                "signal": { "id": { "number": 15, "set": true, "infinite": false }, "name": "SIGTERM" },
                "status": ["SIGNALED"]
            },
            "required": {
                "CPUs": 1,
                "memory_per_cpu": { "number": 0, "set": false, "infinite": false },
                "memory_per_node": { "number": 5120, "set": true, "infinite": false }
            },
            "array": {
                "job_id": 0,
                "task_id": { "number": 0, "set": false, "infinite": false }
            },
            "time": {
                "submission": 1744200000_u64,
                "start": 1744200010_u64,
                "end": 1744243200_u64,
                "elapsed": 43190,
                "limit": { "number": 720, "set": true, "infinite": false },
                "total": { "seconds": 100, "microseconds": 500000 },
                "system": { "seconds": 10, "microseconds": 0 },
                "user": { "seconds": 90, "microseconds": 500000 }
            },
            "tres": {
                "allocated": [
                    { "type": "cpu", "count": 1 },
                    { "type": "billing", "count": 2504 },
                    { "type": "mem", "count": 5368709120_u64 },
                    { "type": "node", "count": 1 }
                ]
            },
            "steps": [
                {
                    "step": { "name": "batch" },
                    "tres": {
                        "requested": {
                            "max": [
                                { "type": "mem", "count": 268435456_u64 }
                            ]
                        }
                    }
                },
                {
                    "step": { "name": "extern" },
                    "tres": {
                        "requested": {
                            "max": [
                                { "type": "mem", "count": 1048576_u64 }
                            ]
                        }
                    }
                }
            ]
        });
        let job = parse_job_record(&jv).unwrap();
        assert_eq!(job.state, "TIMEOUT");
        assert_eq!(job.exit_code, "0:15");
        assert_eq!(job.elapsed_secs, 43190);
        assert_eq!(job.walltime_requested_secs, 720 * 60);
        assert_eq!(job.req_mem_bytes, 5120 * 1024 * 1024);
        assert_eq!(job.max_rss_bytes, 268435456);
        assert_eq!(job.total_cpu_secs, 101);
        assert_eq!(job.user_cpu_secs, 91);
        assert!((job.billing - 2504.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_job_record_failed_nonzero_return() {
        let jv = serde_json::json!({
            "job_id": 14464136,
            "name": "failed_job",
            "user": "testuser",
            "group": "testgrp",
            "account": "testacct",
            "partition": "gpu",
            "cluster": "armis2",
            "nodes": "armis20108",
            "allocation_nodes": 1,
            "submit_line": "sbatch -D /scratch/workdir --export NONE --parsable -M armis2",
            "working_directory": "/scratch/workdir",
            "stdout_expanded": "/scratch/workdir/out.log",
            "stderr_expanded": "/scratch/workdir/err.log",
            "state": { "current": ["FAILED"], "reason": "NonZeroExitCode" },
            "exit_code": {
                "return_code": { "number": 1, "set": true, "infinite": false },
                "signal": { "id": { "number": 0, "set": false, "infinite": false }, "name": "" },
                "status": ["FAILED"]
            },
            "required": {
                "CPUs": 8,
                "memory_per_cpu": { "number": 8192, "set": true, "infinite": false },
                "memory_per_node": { "number": 0, "set": false, "infinite": false }
            },
            "array": {
                "job_id": 0,
                "task_id": { "number": 0, "set": false, "infinite": false }
            },
            "time": {
                "submission": 1743000000_u64,
                "start": 1743000005_u64,
                "end": 1743000015_u64,
                "elapsed": 10,
                "limit": { "number": 60, "set": true, "infinite": false },
                "total": { "seconds": 0, "microseconds": 400000 },
                "system": { "seconds": 0, "microseconds": 200000 },
                "user": { "seconds": 0, "microseconds": 200000 }
            },
            "tres": {
                "allocated": [
                    { "type": "cpu", "count": 8 },
                    { "type": "billing", "count": 120138 },
                    { "type": "mem", "count": 68719476736_u64 },
                    { "type": "gres/gpu", "count": 1 }
                ]
            },
            "steps": []
        });
        let job = parse_job_record(&jv).unwrap();
        assert_eq!(job.state, "FAILED");
        assert_eq!(job.exit_code, "1");
        assert_eq!(job.alloc_cpus, 8);
        assert_eq!(job.req_mem_bytes, 8192 * 1024 * 1024 * 8);
        assert!(job.req_mem_per_cpu);
        assert_eq!(job.total_cpu_secs, 0);
        assert_eq!(job.user_cpu_secs, 0);
        assert_eq!(job.system_cpu_secs, 0);
    }

    #[test]
    fn test_parse_max_rss_multi_step() {
        let jv = serde_json::json!({
            "steps": [
                {
                    "step": { "name": "batch" },
                    "tres": {
                        "requested": {
                            "max": [
                                { "type": "mem", "count": 500000000_u64 }
                            ]
                        }
                    }
                },
                {
                    "step": { "name": "extern" },
                    "tres": {
                        "requested": {
                            "max": [
                                { "type": "mem", "count": 1048576_u64 }
                            ]
                        }
                    }
                }
            ]
        });
        let rss = parse_max_rss(&jv);
        assert_eq!(rss, 500000000);
    }

    #[test]
    fn test_resolve_script_path_ondemand_submit_line() {
        // "sbatch --parsable" has no script
        assert_eq!(resolve_script_path("sbatch --parsable", "/home/user"), None);
    }

    #[test]
    fn test_resolve_script_path_armis2_complex() {
        assert_eq!(
            resolve_script_path(
                "sbatch -D /work/dir --export NONE --parsable -M armis2",
                "/home/user"
            ),
            None
        );
    }

    #[test]
    fn test_sub_second_cpu_time_rounding() {
        // 499999µs rounds to 0s, not 1s
        let jv = serde_json::json!({
            "job_id": 999,
            "name": "fast",
            "user": "u",
            "group": "g",
            "account": "a",
            "partition": "standard",
            "cluster": "c",
            "nodes": "n1",
            "allocation_nodes": 1,
            "submit_line": "",
            "stdout_expanded": "",
            "stderr_expanded": "",
            "state": { "current": ["COMPLETED"] },
            "exit_code": {
                "return_code": { "number": 0, "set": true, "infinite": false },
                "signal": { "id": { "number": 0, "set": false, "infinite": false }, "name": "" },
                "status": ["SUCCESS"]
            },
            "required": {
                "CPUs": 1,
                "memory_per_cpu": { "number": 0, "set": false, "infinite": false },
                "memory_per_node": { "number": 100, "set": true, "infinite": false }
            },
            "array": { "job_id": 0, "task_id": { "number": 0, "set": false, "infinite": false } },
            "time": {
                "submission": 1000_u64,
                "start": 1000_u64,
                "end": 1001_u64,
                "elapsed": 1,
                "limit": { "number": 1, "set": true, "infinite": false },
                "total": { "seconds": 0, "microseconds": 499999 },
                "system": { "seconds": 0, "microseconds": 100000 },
                "user": { "seconds": 0, "microseconds": 399999 }
            },
            "tres": { "allocated": [] },
            "steps": []
        });
        let job = parse_job_record(&jv).unwrap();
        assert_eq!(job.total_cpu_secs, 0);
        assert_eq!(job.user_cpu_secs, 0);
        assert_eq!(job.system_cpu_secs, 0);
    }
}
