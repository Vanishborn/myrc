use std::collections::BTreeMap;
use std::env;

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use clap::Args as ClapArgs;
use colored::Colorize;
use serde::Serialize;

use crate::common::{
    Align, ClusterEnv, Column, DIVIDER, FiscalYear, OutputMode, SpinnerGroup, SpinnerKind, Table,
    billing_divisor, color_info, color_success, format_dollars, parse_slurm_kv, slurm_cmd,
    slurm_cmd_parallel, validate_account,
};

/// Arguments for `myrc account usage`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Account to report.
    #[arg(short, long, alias = "A")]
    pub account: String,

    /// Fiscal year: integer, `this`, or `last`.
    #[arg(short, long, alias = "Y")]
    pub year: Option<String>,

    /// Start month `YYYY-MM`. Requires `--end`.
    #[arg(short, long, alias = "S")]
    pub start: Option<String>,

    /// End month `YYYY-MM`. Requires `--start`.
    #[arg(short, long, alias = "E")]
    pub end: Option<String>,

    /// Show per-user percentage columns.
    #[arg(short, long)]
    pub percentage: bool,

    /// TRES type: `billing`, `cpu`, or `gpu`.
    #[arg(short = 't', long = "type", default_value = "billing")]
    pub tres_type: String,

    /// Sort users by total across range (default).
    #[arg(long, group = "sort")]
    pub sort_by_total: bool,

    /// Sort users by current (latest) month.
    #[arg(long, group = "sort")]
    pub sort_by_current: bool,

    /// Sort users by previous (second-to-last) month.
    #[arg(long, group = "sort")]
    pub sort_by_previous: bool,

    /// Sort users alphabetically by login (uniqname).
    #[arg(long, group = "sort")]
    pub sort_by_user: bool,
}

#[derive(Debug, Serialize)]
struct AccountUsageJson {
    module: &'static str,
    version: &'static str,
    account: String,
    cluster: Option<String>,
    tres_type: String,
    billing_divisor: u64,
    start_date: String,
    end_date: String,
    account_limit: Option<LimitJson>,
    current_month_total: Option<MonthTotalJson>,
    months: Vec<String>,
    users: Vec<UserJson>,
}

#[derive(Debug, Serialize)]
struct LimitJson {
    raw: u64,
    dollars: f64,
}

#[derive(Debug, Serialize)]
struct MonthTotalJson {
    raw: u64,
    dollars: f64,
}

#[derive(Debug, Serialize)]
struct UserJson {
    login: String,
    name: String,
    by_month: Vec<Option<f64>>,
    total: f64,
    percent_of_account: f64,
}

/// Per-user billing data accumulated across months.
struct UserData {
    name: String,
    by_month: Vec<f64>,
    total: f64,
}

enum SortBy {
    Total,
    Current,
    Previous,
    User,
}

fn sort_key(args: &Args) -> SortBy {
    if args.sort_by_user {
        SortBy::User
    } else if args.sort_by_current {
        SortBy::Current
    } else if args.sort_by_previous {
        SortBy::Previous
    } else {
        SortBy::Total
    }
}

pub async fn run(args: &Args, output_mode: OutputMode) -> Result<()> {
    let cluster = ClusterEnv::from_env().ok();

    // Resolve TRES type (Lighthouse: billing → cpu)
    let mut tres = match args.tres_type.to_lowercase().as_str() {
        "cpu" => "cpu".to_string(),
        "gpu" => "gres/gpu".to_string(),
        _ => "billing".to_string(),
    };
    let tres_display = args.tres_type.to_lowercase();
    if let Some(ref c) = cluster {
        if c.is_lighthouse() && tres == "billing" {
            tres = "cpu".to_string();
        }
    }

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

    // Determine month list
    let now = Local::now().date_naive();
    let months = resolve_months(args, now)?;
    let num_months = months.len();

    // Compute divisor per month
    let divisor_override = env::var("MY_ACCOUNT_DIVISOR")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|&d| d > 1.0);

    let divisors: Vec<f64> = months
        .iter()
        .map(|&(y, m)| {
            if let Some(d) = divisor_override {
                d
            } else if tres_display != "billing" {
                60.0 // CPU/GPU: minutes → hours
            } else {
                let date = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
                billing_divisor(&date) as f64
            }
        })
        .collect();

    // The "canonical" divisor for JSON output is the first month's
    let canonical_divisor = if let Some(d) = divisor_override {
        d as u64
    } else if tres_display != "billing" {
        60
    } else {
        let date = NaiveDate::from_ymd_opt(months[0].0, months[0].1, 1).unwrap();
        billing_divisor(&date)
    };

    // Phase 2: fetch account limit (GrpTRESMins)
    let account_limit = fetch_account_limit(&args.account, &tres_display).await;

    // Phase 3: concurrent month queries with spinners
    let cmds: Vec<Vec<String>> = months
        .iter()
        .map(|&(y, m)| {
            let start = format!("{y}-{m:02}-01");
            let end = if m == 12 {
                format!("{}-01-01", y + 1)
            } else {
                format!("{y}-{:02}-01", m + 1)
            };
            vec![
                "sreport".into(),
                "-n".into(),
                "-P".into(),
                format!("--tres={tres}"),
                "cluster".into(),
                "AccountUtilizationByUser".into(),
                format!("Accounts={}", args.account),
                format!("Start={start}"),
                format!("End={end}"),
                "format=account,login,proper,used".into(),
            ]
        })
        .collect();

    if !output_mode.is_json() {
        eprintln!("\n{}", format!("Querying {} months...", num_months).bold());
    }

    // Spinners (stderr only, suppressed in JSON mode)
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
        sp.set_message(format!("0/{num_months} months"));
    }

    let results = slurm_cmd_parallel(cmds).await?;

    // Update spinners as done
    if let Some(ref sp) = total_spinner {
        sp.set_message(format!("{num_months}/{num_months} months"));
    }
    if let Some(ref sp) = success_spinner {
        sp.set_message(format!("{num_months}/{num_months} months"));
    }
    spinner_group.finish();

    if !output_mode.is_json() {
        eprintln!("{}\n", color_success("Done."));
    }

    // Phase 4: parse results into per-user data
    // BTreeMap keeps users sorted by login for deterministic output
    let mut users: BTreeMap<String, UserData> = BTreeMap::new();
    let mut totals_by_month: Vec<f64> = vec![0.0; num_months];
    let mut grand_total: f64 = 0.0;

    for (month_idx, output) in results.iter().enumerate() {
        let rows = parse_slurm_kv(output);
        for row in &rows {
            if row.len() < 4 {
                continue;
            }
            let login = row[1].trim();
            // sreport emits a summary row with an empty login field for the
            // account-level total.  Skip it so we don't double-count.
            if login.is_empty() {
                continue;
            }
            let name = row[2].trim();
            let raw: f64 = row[3].trim().parse().unwrap_or(0.0);
            let value = raw / divisors[month_idx];

            let entry = users.entry(login.to_string()).or_insert_with(|| UserData {
                name: name.to_string(),
                by_month: vec![0.0; num_months],
                total: 0.0,
            });
            entry.by_month[month_idx] = value;
            entry.total += value;

            totals_by_month[month_idx] += value;
            grand_total += value;
        }
    }

    // Check for "as of" flag: current month is included
    let is_current_month = months
        .iter()
        .any(|&(y, m)| y == now.year() && m == now.month());

    // Compute month labels
    let month_labels: Vec<String> = months.iter().map(|&(y, m)| format!("{y}-{m:02}")).collect();

    // Compute start/end date strings
    let start_date = format!("{}-{:02}-01", months[0].0, months[0].1);
    let last = months.last().unwrap();
    let end_date = if last.1 == 12 {
        format!("{}-01-01", last.0 + 1)
    } else {
        format!("{}-{:02}-01", last.0, last.1 + 1)
    };

    // Sort users
    let mut sorted_users: Vec<(String, &UserData)> =
        users.iter().map(|(k, v)| (k.clone(), v)).collect();
    let sort_by = sort_key(args);
    sorted_users.sort_by(|a, b| {
        if matches!(sort_by, SortBy::User) {
            return a.0.cmp(&b.0);
        }
        let val_a = match sort_by {
            SortBy::Total => a.1.total,
            SortBy::Current => *a.1.by_month.last().unwrap_or(&0.0),
            SortBy::Previous => {
                if a.1.by_month.len() >= 2 {
                    a.1.by_month[a.1.by_month.len() - 2]
                } else {
                    0.0
                }
            }
            SortBy::User => unreachable!(),
        };
        let val_b = match sort_by {
            SortBy::Total => b.1.total,
            SortBy::Current => *b.1.by_month.last().unwrap_or(&0.0),
            SortBy::Previous => {
                if b.1.by_month.len() >= 2 {
                    b.1.by_month[b.1.by_month.len() - 2]
                } else {
                    0.0
                }
            }
            SortBy::User => unreachable!(),
        };
        val_b
            .partial_cmp(&val_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // JSON output
    if output_mode.is_json() {
        return print_json(
            &args.account,
            cluster.as_ref().map(|c| c.name.clone()),
            &tres_display,
            canonical_divisor,
            &start_date,
            &end_date,
            &account_limit,
            &month_labels,
            &sorted_users,
            &totals_by_month,
            grand_total,
            now,
        );
    }

    // Human-readable output
    // Header
    println!(
        "{}{}",
        "Account Usage: ".bold(),
        color_info(&args.account).bold()
    );
    println!("{DIVIDER}");
    println!("{:<20} {}", "Report type:", tres_display);
    if tres_display != "billing" {
        println!("{:<20} {tres_display}*hr", "Units:");
    }
    print!("{:<20} {} to {}", "Period:", start_date, end_date);
    if is_current_month {
        print!(
            "\n{:<20} This month is an estimate as of {}",
            "",
            Local::now().format("%Y-%m-%d %H:%M:%S")
        );
    }
    println!();

    // Account limit line
    if let Some((_raw, limit_dollars)) = &account_limit {
        let used_this_month = totals_by_month.last().copied().unwrap_or(0.0);
        println!(
            "{} has used approximately {} of an allowed {} limit this month",
            args.account,
            format_dollars(used_this_month).bold(),
            format_dollars(*limit_dollars).bold()
        );
    }

    // Build table
    let mut columns: Vec<Column> = vec![
        Column {
            header: "user".into(),
            align: Align::Left,
        },
        Column {
            header: "Name".into(),
            align: Align::Left,
        },
    ];
    for label in &month_labels {
        columns.push(Column {
            header: label.clone(),
            align: Align::Right,
        });
        if args.percentage {
            columns.push(Column {
                header: "%".into(),
                align: Align::Right,
            });
        }
    }
    columns.push(Column {
        header: "Total".into(),
        align: Align::Right,
    });
    if args.percentage {
        columns.push(Column {
            header: "%".into(),
            align: Align::Right,
        });
    }

    let mut table = Table::new(columns);

    // "total" row first
    let mut total_row = vec!["total".to_string(), String::new()];
    for &val in totals_by_month.iter() {
        total_row.push(format!("{val:.2}"));
        if args.percentage {
            total_row.push("100%".into());
        }
    }
    total_row.push(format!("{grand_total:.2}"));
    if args.percentage {
        total_row.push("100%".into());
    }
    table.add_row(total_row);

    // Per-user rows
    for (login, data) in &sorted_users {
        let mut row = vec![login.clone(), data.name.clone()];
        for (i, &val) in data.by_month.iter().enumerate() {
            row.push(format!("{val:.2}"));
            if args.percentage {
                let pct = if totals_by_month[i] > 0.0 {
                    (100.0 * val) / totals_by_month[i]
                } else {
                    0.0
                };
                row.push(format!("{pct:.0}%"));
            }
        }
        row.push(format!("{:.2}", data.total));
        if args.percentage {
            let pct = if grand_total > 0.0 {
                (100.0 * data.total) / grand_total
            } else {
                0.0
            };
            row.push(format!("{pct:.0}%"));
        }
        table.add_row(row);
    }

    print!("{table}");
    Ok(())
}

/// Resolve the list of (year, month) pairs to query based on args.
fn resolve_months(args: &Args, now: NaiveDate) -> Result<Vec<(i32, u32)>> {
    if let (Some(start), Some(end)) = (&args.start, &args.end) {
        // Arbitrary range: -s YYYY-MM -e YYYY-MM
        let (sy, sm) = parse_ym(start)?;
        let (ey, em) = parse_ym(end)?;
        let mut months = Vec::new();
        let mut y = sy;
        let mut m = sm;
        loop {
            months.push((y, m));
            if y == ey && m == em {
                break;
            }
            m += 1;
            if m > 12 {
                m = 1;
                y += 1;
            }
            if months.len() > 120 {
                anyhow::bail!("date range too large (>120 months)");
            }
        }
        return Ok(months);
    }

    if let Some((Some(_), None)) | Some((None, Some(_))) =
        Some((&args.start, &args.end)).map(|(a, b)| (a.as_ref(), b.as_ref()))
    {
        anyhow::bail!("--start and --end must both be provided");
    }

    if let Some(ref year_str) = args.year {
        let fy = match year_str.to_lowercase().as_str() {
            "this" => FiscalYear::current(),
            "last" => FiscalYear::previous(),
            s => {
                let y: i32 = s.parse().context("invalid year")?;
                FiscalYear::from_year(y)
            }
        };
        // If fiscal year is current, only include months up to now
        let all = fy.months();
        let months: Vec<(i32, u32)> = all
            .into_iter()
            .filter(|&(y, m)| {
                let d = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
                d <= now
            })
            .collect();
        if months.is_empty() {
            anyhow::bail!("no months available for the specified fiscal year");
        }
        return Ok(months);
    }

    // Default: last month + current month
    let cur = (now.year(), now.month());
    let prev = if now.month() == 1 {
        (now.year() - 1, 12)
    } else {
        (now.year(), now.month() - 1)
    };
    Ok(vec![prev, cur])
}

/// Parse `YYYY-MM` into `(year, month)`.
fn parse_ym(s: &str) -> Result<(i32, u32)> {
    let parts: Vec<&str> = s.split('-').collect();
    anyhow::ensure!(parts.len() == 2, "expected YYYY-MM, got '{s}'");
    let y: i32 = parts[0].parse().context("invalid year")?;
    let m: u32 = parts[1].parse().context("invalid month")?;
    anyhow::ensure!((1..=12).contains(&m), "month out of range in '{s}'");
    Ok((y, m))
}

/// Fetch the account GrpTRESMins limit (if set). Returns `(raw, dollars)`.
async fn fetch_account_limit(account: &str, tres_type: &str) -> Option<(u64, f64)> {
    let output = slurm_cmd(&[
        "sacctmgr",
        "-n",
        "-p",
        "show",
        "assoc",
        &format!("account={account}"),
        "format=GrpTRESMins",
    ])
    .await
    .ok()?;

    // Look for `billing=NNNN` in the GrpTRESMins field
    let target = if tres_type == "billing" {
        "billing="
    } else if tres_type == "cpu" {
        "cpu="
    } else {
        "gres/gpu="
    };

    for line in output.lines() {
        let trimmed = line.strip_suffix('|').unwrap_or(line);
        for pair in trimmed.split(',') {
            if let Some(rest) = pair.strip_prefix(target) {
                if let Ok(raw) = rest.trim().parse::<u64>() {
                    let now = Local::now().date_naive();
                    let div = billing_divisor(&now) as f64;
                    return Some((raw, raw as f64 / div));
                }
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn print_json(
    account: &str,
    cluster: Option<String>,
    tres_type: &str,
    divisor: u64,
    start_date: &str,
    end_date: &str,
    account_limit: &Option<(u64, f64)>,
    month_labels: &[String],
    sorted_users: &[(String, &UserData)],
    totals_by_month: &[f64],
    grand_total: f64,
    now: NaiveDate,
) -> Result<()> {
    let limit_json = account_limit
        .as_ref()
        .map(|&(raw, dollars)| LimitJson { raw, dollars });

    // Current month total
    let current_total = totals_by_month.last().map(|&val| {
        let raw = (val * divisor as f64) as u64;
        MonthTotalJson { raw, dollars: val }
    });

    let users: Vec<UserJson> = sorted_users
        .iter()
        .map(|(login, data)| {
            let by_month: Vec<Option<f64>> = data
                .by_month
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    // Mark future months as null
                    let label = &month_labels[i];
                    let parts: Vec<&str> = label.split('-').collect();
                    if parts.len() == 2 {
                        let y: i32 = parts[0].parse().unwrap_or(0);
                        let m: u32 = parts[1].parse().unwrap_or(0);
                        if y > now.year() || (y == now.year() && m > now.month()) {
                            return None;
                        }
                    }
                    Some(v)
                })
                .collect();
            let pct = if grand_total > 0.0 {
                (100.0 * data.total) / grand_total
            } else {
                0.0
            };
            UserJson {
                login: login.clone(),
                name: data.name.clone(),
                by_month,
                total: data.total,
                percent_of_account: (pct * 10.0).round() / 10.0,
            }
        })
        .collect();

    let json = AccountUsageJson {
        module: "account_usage",
        version: env!("CARGO_PKG_VERSION"),
        account: account.to_string(),
        cluster,
        tres_type: tres_type.to_string(),
        billing_divisor: divisor,
        start_date: start_date.to_string(),
        end_date: end_date.to_string(),
        account_limit: limit_json,
        current_month_total: current_total,
        months: month_labels.to_vec(),
        users,
    };
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ym_valid() {
        assert_eq!(parse_ym("2025-07").unwrap(), (2025, 7));
        assert_eq!(parse_ym("2026-01").unwrap(), (2026, 1));
    }

    #[test]
    fn parse_ym_invalid() {
        assert!(parse_ym("2025").is_err());
        assert!(parse_ym("2025-13").is_err());
        assert!(parse_ym("2025-00").is_err());
    }

    #[test]
    fn resolve_months_default() {
        let args = Args {
            account: "test".into(),
            year: None,
            start: None,
            end: None,
            percentage: false,
            tres_type: "billing".into(),
            sort_by_total: false,
            sort_by_current: false,
            sort_by_previous: false,
            sort_by_user: false,
        };
        let now = NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        let months = resolve_months(&args, now).unwrap();
        assert_eq!(months, vec![(2026, 3), (2026, 4)]);
    }

    #[test]
    fn resolve_months_january_wraps() {
        let args = Args {
            account: "test".into(),
            year: None,
            start: None,
            end: None,
            percentage: false,
            tres_type: "billing".into(),
            sort_by_total: false,
            sort_by_current: false,
            sort_by_previous: false,
            sort_by_user: false,
        };
        let now = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let months = resolve_months(&args, now).unwrap();
        assert_eq!(months, vec![(2025, 12), (2026, 1)]);
    }

    #[test]
    fn resolve_months_range() {
        let args = Args {
            account: "test".into(),
            year: None,
            start: Some("2025-10".into()),
            end: Some("2026-02".into()),
            percentage: false,
            tres_type: "billing".into(),
            sort_by_total: false,
            sort_by_current: false,
            sort_by_previous: false,
            sort_by_user: false,
        };
        let now = NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        let months = resolve_months(&args, now).unwrap();
        assert_eq!(
            months,
            vec![(2025, 10), (2025, 11), (2025, 12), (2026, 1), (2026, 2)]
        );
    }

    #[test]
    fn resolve_months_fiscal_year() {
        let args = Args {
            account: "test".into(),
            year: Some("2025".into()),
            start: None,
            end: None,
            percentage: false,
            tres_type: "billing".into(),
            sort_by_total: false,
            sort_by_current: false,
            sort_by_previous: false,
            sort_by_user: false,
        };
        let now = NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        let months = resolve_months(&args, now).unwrap();
        // FY2025 = Jul 2025 – Jun 2026, but filtered to <= now (Apr 2026)
        assert_eq!(months.len(), 10); // Jul..Apr
        assert_eq!(months[0], (2025, 7));
        assert_eq!(months[9], (2026, 4));
    }
}
