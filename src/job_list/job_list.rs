use anyhow::{Context, Result};
use chrono::Datelike;
use clap::Args as ClapArgs;
use serde::Serialize;

use crate::common::{
    Align, Column, OutputMode, Table, color_dim, format_memory, parse_slurm_kv, resolve_user,
    slurm_cmd,
};

/// List and filter a user's jobs.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// User to query.
    #[arg(short, long)]
    pub user: Option<String>,

    /// Filter by year.
    #[arg(short, long)]
    pub year: Option<u16>,

    /// Filter by month (1-12).
    #[arg(short, long)]
    pub month: Option<u8>,

    /// Filter by day (1-31).
    #[arg(short, long)]
    pub day: Option<u8>,

    /// Filter by state: completed, failed, timeout, running, cancelled, oom.
    #[arg(short = 't', long = "type")]
    pub state_type: Option<String>,

    /// Filter by account.
    #[arg(short, long)]
    pub account: Option<String>,

    /// Max rows to show.
    #[arg(short = 'n', long, default_value_t = 25)]
    pub limit: u32,

    /// Sort key: submit, start, end, id.
    #[arg(long, default_value = "submit")]
    pub sort_by: String,

    /// Reverse sort order.
    #[arg(long)]
    pub reverse: bool,
}

#[derive(Serialize)]
struct JobListJson {
    module: &'static str,
    version: &'static str,
    user: String,
    cluster: Option<String>,
    filters: FiltersJson,
    sort_by: String,
    limit: u32,
    total_matching: usize,
    jobs: Vec<JobEntryJson>,
}

#[derive(Serialize)]
struct FiltersJson {
    start_date: String,
    end_date: String,
    state: Option<String>,
    account: Option<String>,
}

#[derive(Serialize)]
struct JobEntryJson {
    job_id: String,
    job_name: String,
    account: String,
    state: String,
    submit_time: String,
    start_time: String,
    end_time: String,
    elapsed_seconds: u64,
    elapsed_slurm: String,
    alloc_cpus: u32,
    req_mem_bytes: u64,
}

fn map_state(s: &str) -> Option<&'static str> {
    match s.to_ascii_lowercase().as_str() {
        "completed" => Some("COMPLETED"),
        "failed" => Some("FAILED"),
        "timeout" => Some("TIMEOUT"),
        "running" => Some("RUNNING"),
        "cancelled" | "canceled" => Some("CANCELLED"),
        "oom" | "out_of_memory" => Some("OUT_OF_MEMORY"),
        "pending" => Some("PENDING"),
        _ => None,
    }
}

pub async fn run(args: &Args, output_mode: OutputMode) -> Result<()> {
    let user = resolve_user(None, args.user.as_deref()).context("resolving user")?;

    // Build date range for sacct
    let (start_time, end_time) = build_date_range(args)?;

    // Build sacct command
    let mut cmd = vec![
        "sacct".to_string(),
        "-u".to_string(),
        user.clone(),
        "-n".to_string(),
        "-X".to_string(),
        "-P".to_string(),
        "--noconvert".to_string(),
        format!("--format=JobID,JobName,Account,State,Submit,Start,End,Elapsed,AllocCPUS,ReqMem"),
        format!("-S{start_time}"),
        format!("-E{end_time}"),
    ];

    // State filter
    let slurm_state = if let Some(ref st) = args.state_type {
        let mapped = map_state(st)
			.ok_or_else(|| anyhow::anyhow!(
				"invalid state type '{}'. Valid: completed, failed, timeout, running, cancelled, oom",
				st
			))?;
        cmd.push(format!("--state={mapped}"));
        Some(st.clone())
    } else {
        None
    };

    // Account filter
    if let Some(ref acct) = args.account {
        cmd.push(format!("-A{acct}"));
    }

    let output = slurm_cmd(&cmd)
        .await
        .map_err(|e| anyhow::anyhow!("sacct failed: {e}"))?;

    let rows = parse_slurm_kv(&output);
    let mut jobs: Vec<JobRow> = rows
        .iter()
        .filter(|r| r.len() >= 10)
        .map(|r| JobRow {
            job_id: r[0].to_string(),
            job_name: r[1].to_string(),
            account: r[2].to_string(),
            state: r[3].to_string(),
            submit: r[4].to_string(),
            start: r[5].to_string(),
            end: r[6].to_string(),
            elapsed: r[7].to_string(),
            alloc_cpus: r[8].to_string(),
            req_mem: r[9].to_string(),
        })
        .collect();

    let total_matching = jobs.len();

    if jobs.is_empty() && !output_mode.is_json() {
        eprintln!("{}", color_dim("No jobs found matching criteria."));
    }

    // Sort
    sort_jobs(&mut jobs, &args.sort_by, args.reverse);

    // Truncate
    jobs.truncate(args.limit as usize);

    if output_mode.is_json() {
        print_json(
            args,
            &user,
            &start_time,
            &end_time,
            slurm_state,
            total_matching,
            &jobs,
        );
    } else if !jobs.is_empty() {
        print_table(&jobs);
    }

    Ok(())
}

fn build_date_range(
    args: &Args,
) -> std::result::Result<(String, String), crate::common::MyrcError> {
    use chrono::NaiveDate;

    let now = chrono::Local::now().date_naive();
    let year = args.year.map(|y| y as i32).unwrap_or(now.year());
    let month = args.month;
    let day = args.day;

    // Default: last 7 days
    if args.year.is_none() && month.is_none() && day.is_none() {
        let start = now - chrono::Duration::days(7);
        return Ok((
            start.format("%Y-%m-%d").to_string(),
            now.format("%Y-%m-%d").to_string(),
        ));
    }

    let start_month = month.unwrap_or(1) as u32;
    let end_month = month.unwrap_or(12) as u32;

    let start_day = day.unwrap_or(1) as u32;
    let end_day = if let Some(d) = day {
        let d = d as u32;
        // Validate the day is real for this month/year
        if NaiveDate::from_ymd_opt(year, end_month, d).is_none() {
            return Err(crate::common::MyrcError::InvalidInput(format!(
                "day {d} is not valid for {year:04}-{end_month:02}",
            )));
        }
        d
    } else {
        last_day(year, end_month)
    };

    let start = format!("{year:04}-{start_month:02}-{start_day:02}");
    let end = format!("{year:04}-{end_month:02}-{end_day:02}");
    Ok((start, end))
}

fn last_day(year: i32, month: u32) -> u32 {
    use chrono::NaiveDate;
    if month == 12 {
        31
    } else {
        let next = NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap();
        (next - chrono::Duration::days(1)).day()
    }
}

struct JobRow {
    job_id: String,
    job_name: String,
    account: String,
    state: String,
    submit: String,
    start: String,
    end: String,
    elapsed: String,
    alloc_cpus: String,
    req_mem: String,
}

fn sort_jobs(jobs: &mut [JobRow], sort_by: &str, reverse: bool) {
    jobs.sort_by(|a, b| {
        let cmp = match sort_by {
            "start" => a.start.cmp(&b.start),
            "end" => a.end.cmp(&b.end),
            "id" => {
                let a_id: u64 = a
                    .job_id
                    .split('_')
                    .next()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0);
                let b_id: u64 = b
                    .job_id
                    .split('_')
                    .next()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0);
                a_id.cmp(&b_id)
            }
            _ => a.submit.cmp(&b.submit), // "submit" is default
        };
        if reverse { cmp } else { cmp.reverse() }
    });
}

fn print_table(jobs: &[JobRow]) {
    let mut table = Table::new(vec![
        Column {
            header: "JobID".into(),
            align: Align::Left,
        },
        Column {
            header: "Name".into(),
            align: Align::Left,
        },
        Column {
            header: "Account".into(),
            align: Align::Left,
        },
        Column {
            header: "State".into(),
            align: Align::Left,
        },
        Column {
            header: "Submitted".into(),
            align: Align::Left,
        },
        Column {
            header: "Elapsed".into(),
            align: Align::Right,
        },
        Column {
            header: "CPUs".into(),
            align: Align::Right,
        },
        Column {
            header: "Memory".into(),
            align: Align::Right,
        },
    ]);

    for job in jobs {
        table.add_row(vec![
            job.job_id.clone(),
            truncate_name(&job.job_name, 20),
            job.account.clone(),
            job.state.clone(),
            job.submit.clone(),
            job.elapsed.clone(),
            job.alloc_cpus.clone(),
            format_memory(parse_req_mem_bytes(&job.req_mem)),
        ]);
    }

    print!("{table}");
}

fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        name.to_string()
    } else {
        format!("{}...", &name[..max - 3])
    }
}

fn print_json(
    args: &Args,
    user: &str,
    start_time: &str,
    end_time: &str,
    state: Option<String>,
    total_matching: usize,
    jobs: &[JobRow],
) {
    let cluster = std::env::var("CLUSTER_NAME").ok();
    let json = JobListJson {
        module: "job_list",
        version: env!("CARGO_PKG_VERSION"),
        user: user.to_string(),
        cluster,
        filters: FiltersJson {
            start_date: start_time.to_string(),
            end_date: end_time.to_string(),
            state,
            account: args.account.clone(),
        },
        sort_by: args.sort_by.clone(),
        limit: args.limit,
        total_matching,
        jobs: jobs
            .iter()
            .map(|j| JobEntryJson {
                job_id: j.job_id.clone(),
                job_name: j.job_name.clone(),
                account: j.account.clone(),
                state: j.state.clone(),
                submit_time: j.submit.clone(),
                start_time: j.start.clone(),
                end_time: j.end.clone(),
                elapsed_seconds: parse_elapsed_to_secs(&j.elapsed),
                elapsed_slurm: j.elapsed.clone(),
                alloc_cpus: j.alloc_cpus.parse().unwrap_or(0),
                req_mem_bytes: parse_req_mem_bytes(&j.req_mem),
            })
            .collect(),
    };

    println!("{}", serde_json::to_string_pretty(&json).unwrap());
}

/// Parse sacct elapsed time (HH:MM:SS or D-HH:MM:SS) to total seconds.
fn parse_elapsed_to_secs(s: &str) -> u64 {
    if let Some((days_str, rest)) = s.split_once('-') {
        let days: u64 = days_str.parse().unwrap_or(0);
        days * 86400 + parse_hms_to_secs(rest)
    } else {
        parse_hms_to_secs(s)
    }
}

fn parse_hms_to_secs(s: &str) -> u64 {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        3 => {
            let h: u64 = parts[0].parse().unwrap_or(0);
            let m: u64 = parts[1].parse().unwrap_or(0);
            let sec: u64 = parts[2].parse().unwrap_or(0);
            h * 3600 + m * 60 + sec
        }
        2 => {
            let m: u64 = parts[0].parse().unwrap_or(0);
            let sec: u64 = parts[1].parse().unwrap_or(0);
            m * 60 + sec
        }
        _ => 0,
    }
}

/// Parse sacct ReqMem string (e.g., "4Gn", "768Mc") to bytes.
fn parse_req_mem_bytes(s: &str) -> u64 {
    let s = s.trim();
    // sacct format: "4Gn" (per-node), "768Mc" (per-cpu), "0" etc
    let cleaned = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    if cleaned.is_empty() || cleaned == "0" {
        return 0;
    }

    // Extract unit character before the n/c suffix
    let unit_part = &s[cleaned.len()..];
    let unit = unit_part.chars().next().unwrap_or('M');

    let val: f64 = cleaned.parse().unwrap_or(0.0);
    match unit.to_ascii_uppercase() {
        'T' => (val * (1u64 << 40) as f64) as u64,
        'G' => (val * (1u64 << 30) as f64) as u64,
        'M' => (val * (1u64 << 20) as f64) as u64,
        'K' => (val * (1u64 << 10) as f64) as u64,
        _ => (val * (1u64 << 20) as f64) as u64, // default to MB
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_state() {
        assert_eq!(map_state("completed"), Some("COMPLETED"));
        assert_eq!(map_state("FAILED"), Some("FAILED"));
        assert_eq!(map_state("oom"), Some("OUT_OF_MEMORY"));
        assert_eq!(map_state("cancelled"), Some("CANCELLED"));
        assert_eq!(map_state("canceled"), Some("CANCELLED"));
        assert_eq!(map_state("invalid"), None);
    }

    #[test]
    fn test_parse_elapsed_to_secs() {
        assert_eq!(parse_elapsed_to_secs("01:25:00"), 5100);
        assert_eq!(parse_elapsed_to_secs("1-12:30:00"), 131400);
        assert_eq!(parse_elapsed_to_secs("00:05:30"), 330);
    }

    #[test]
    fn test_parse_req_mem_bytes() {
        assert_eq!(parse_req_mem_bytes("4Gn"), 4 * (1 << 30));
        assert_eq!(parse_req_mem_bytes("768Mc"), 768 * (1 << 20));
        assert_eq!(parse_req_mem_bytes("0"), 0);
    }

    #[test]
    fn test_truncate_name() {
        assert_eq!(truncate_name("short", 20), "short");
        assert_eq!(
            truncate_name("a_very_long_job_name_here", 20),
            "a_very_long_job_n..."
        );
    }

    #[test]
    fn test_sort_jobs_by_id() {
        let mut jobs = vec![
            JobRow {
                job_id: "123".into(),
                job_name: "a".into(),
                account: "x".into(),
                state: "COMPLETED".into(),
                submit: "2026-01-01".into(),
                start: "2026-01-01".into(),
                end: "2026-01-01".into(),
                elapsed: "00:01:00".into(),
                alloc_cpus: "1".into(),
                req_mem: "1Gn".into(),
            },
            JobRow {
                job_id: "456".into(),
                job_name: "b".into(),
                account: "x".into(),
                state: "COMPLETED".into(),
                submit: "2026-01-02".into(),
                start: "2026-01-02".into(),
                end: "2026-01-02".into(),
                elapsed: "00:02:00".into(),
                alloc_cpus: "2".into(),
                req_mem: "2Gn".into(),
            },
        ];
        // Default: newest first (reverse chronological)
        sort_jobs(&mut jobs, "id", false);
        assert_eq!(jobs[0].job_id, "456");
        assert_eq!(jobs[1].job_id, "123");
    }

    #[test]
    fn test_build_date_range_default() {
        let args = Args {
            user: None,
            year: None,
            month: None,
            day: None,
            state_type: None,
            account: None,
            limit: 25,
            sort_by: "submit".into(),
            reverse: false,
        };
        let (start, end) = build_date_range(&args).unwrap();
        assert_eq!(start.len(), 10);
        assert_eq!(end.len(), 10);
    }

    #[test]
    fn test_build_date_range_feb_leap_year() {
        // 2024 is a leap year, Feb has 29 days
        let args = Args {
            user: None,
            year: Some(2024),
            month: Some(2),
            day: None,
            state_type: None,
            account: None,
            limit: 25,
            sort_by: "submit".into(),
            reverse: false,
        };
        let (start, end) = build_date_range(&args).unwrap();
        assert_eq!(start, "2024-02-01");
        assert_eq!(end, "2024-02-29");
    }

    #[test]
    fn test_build_date_range_feb_non_leap_year() {
        // 2026 is not a leap year, Feb has 28 days
        let args = Args {
            user: None,
            year: Some(2026),
            month: Some(2),
            day: None,
            state_type: None,
            account: None,
            limit: 25,
            sort_by: "submit".into(),
            reverse: false,
        };
        let (start, end) = build_date_range(&args).unwrap();
        assert_eq!(start, "2026-02-01");
        assert_eq!(end, "2026-02-28");
    }

    #[test]
    fn test_build_date_range_specific_day() {
        let args = Args {
            user: None,
            year: Some(2026),
            month: Some(2),
            day: Some(15),
            state_type: None,
            account: None,
            limit: 25,
            sort_by: "submit".into(),
            reverse: false,
        };
        let (start, end) = build_date_range(&args).unwrap();
        assert_eq!(start, "2026-02-15");
        assert_eq!(end, "2026-02-15");
    }

    #[test]
    fn test_invalid_feb_29_non_leap() {
        let args = Args {
            user: None,
            year: Some(2026),
            month: Some(2),
            day: Some(29),
            state_type: None,
            account: None,
            limit: 25,
            sort_by: "submit".into(),
            reverse: false,
        };
        assert!(build_date_range(&args).is_err());
    }

    #[test]
    fn test_invalid_feb_30() {
        let args = Args {
            user: None,
            year: Some(2024),
            month: Some(2),
            day: Some(30),
            state_type: None,
            account: None,
            limit: 25,
            sort_by: "submit".into(),
            reverse: false,
        };
        assert!(build_date_range(&args).is_err());
    }

    #[test]
    fn test_invalid_day_31_in_30day_month() {
        // April, June, September, November have only 30 days
        for month in [4, 6, 9, 11] {
            let args = Args {
                user: None,
                year: Some(2026),
                month: Some(month),
                day: Some(31),
                state_type: None,
                account: None,
                limit: 25,
                sort_by: "submit".into(),
                reverse: false,
            };
            assert!(
                build_date_range(&args).is_err(),
                "month {month} should reject day 31"
            );
        }
    }

    #[test]
    fn test_valid_day_30_in_30day_month() {
        let args = Args {
            user: None,
            year: Some(2026),
            month: Some(4),
            day: Some(30),
            state_type: None,
            account: None,
            limit: 25,
            sort_by: "submit".into(),
            reverse: false,
        };
        let (start, end) = build_date_range(&args).unwrap();
        assert_eq!(start, "2026-04-30");
        assert_eq!(end, "2026-04-30");
    }
}
