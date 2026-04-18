use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use clap::Args as ClapArgs;
use colored::Colorize;
use serde::Serialize;

use crate::common::{
    Align, ClusterEnv, Column, DIVIDER, MyrcError, OutputMode, SpinnerGroup, SpinnerKind, Table,
    billing_divisor, format_dollars, parse_slurm_kv, resolve_user, slurm_cmd,
};

/// Arguments for `myrc usage`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// User to query (positional for backward compat).
    #[arg(value_name = "USER")]
    pub user: Option<String>,

    /// User to query (flag form).
    #[arg(short = 'u', long = "user")]
    pub user_flag: Option<String>,

    /// Year to query.
    #[arg(short, long, alias = "Y")]
    pub year: Option<i32>,

    /// Month to query (1-12).
    #[arg(short, long)]
    pub month: Option<u32>,
}

#[derive(Debug, Serialize)]
struct UsageJson {
    module: &'static str,
    version: &'static str,
    user: String,
    cluster: Option<String>,
    year: i32,
    month: u32,
    start_date: String,
    end_date: String,
    billing_divisor: u64,
    accounts: Vec<UsageRow>,
}

#[derive(Debug, Serialize)]
struct UsageRow {
    account: String,
    login: String,
    usage_raw: u64,
    usage_dollars: f64,
}

pub async fn run(args: &Args, output_mode: OutputMode) -> Result<()> {
    // Lighthouse exclusion
    let cluster = ClusterEnv::from_env().ok();
    if let Some(ref c) = cluster {
        if c.is_lighthouse() {
            return Err(MyrcError::SlurmCmd {
                message: "this utility does not calculate usage for the Lighthouse cluster".into(),
                exit_code: crate::common::ExitCode::ServiceUnavailable,
            }
            .into());
        }
    }

    let user = resolve_user(args.user.as_deref(), args.user_flag.as_deref())?;
    let now = Local::now().date_naive();
    let year = args.year.unwrap_or(now.year());
    let month = args.month.unwrap_or(now.month());

    // Validate year/month
    let next_year = now.year() + 1;
    if year < 2000 || year > next_year {
        return Err(
            MyrcError::InvalidInput(format!("year must be between 2000 and {next_year}")).into(),
        );
    }
    if !(1..=12).contains(&month) {
        return Err(MyrcError::InvalidInput("month must be between 1 and 12".into()).into());
    }

    // Compute date range: first day of month → first day of next month
    let start_date = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    let end_date = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap()
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap()
    };
    let start_str = start_date.format("%Y-%m-%d").to_string();
    let end_str = end_date.format("%Y-%m-%d").to_string();

    let divisor = billing_divisor(&start_date);

    // Build sreport command
    let cmd_args = vec![
        "sreport".to_string(),
        "-n".to_string(),
        "-P".to_string(),
        "--tres=billing".to_string(),
        "cluster".to_string(),
        "AccountUtilizationByUser".to_string(),
        format!("User={user}"),
        format!("Start={start_str}"),
        format!("End={end_str}"),
        "format=account,login,used".to_string(),
    ];

    // Single async call to sreport
    let mut spinner_group = SpinnerGroup::new();
    let spinner = if !output_mode.is_json() {
        let sp = spinner_group.add(SpinnerKind::Total, "Querying:");
        sp.set_message("sreport");
        Some(sp.clone())
    } else {
        None
    };

    let output = slurm_cmd(&cmd_args).await.context("querying usage")?;

    if let Some(ref sp) = spinner {
        sp.set_message("done");
    }
    spinner_group.finish();

    let rows = parse_slurm_kv(&output);

    // Parse into (account, login, raw_used) tuples
    let parsed: Vec<(&str, &str, u64)> = rows
        .iter()
        .filter(|r| r.len() >= 3)
        .filter_map(|r| {
            let raw: u64 = r[2].trim().parse().ok()?;
            Some((r[0], r[1], raw))
        })
        .collect();

    if output_mode.is_json() {
        return print_json(
            &user,
            cluster.as_ref().map(|c| c.name.clone()),
            year,
            month,
            &start_str,
            &end_str,
            divisor,
            &parsed,
        );
    }

    // Human-readable table output
    println!("{}", format!("Usage from {start_str} to {end_str}").bold());
    println!("{DIVIDER}");

    let mut table = Table::new(vec![
        Column {
            header: "Account".into(),
            align: Align::Left,
        },
        Column {
            header: "Login".into(),
            align: Align::Left,
        },
        Column {
            header: "Used($)".into(),
            align: Align::Right,
        },
    ]);

    for &(account, login, raw) in &parsed {
        let dollars = raw as f64 / divisor as f64;
        table.add_row(vec![
            account.to_string(),
            login.to_string(),
            format_dollars(dollars),
        ]);
    }

    print!("{table}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn print_json(
    user: &str,
    cluster: Option<String>,
    year: i32,
    month: u32,
    start_date: &str,
    end_date: &str,
    divisor: u64,
    parsed: &[(&str, &str, u64)],
) -> Result<()> {
    let accounts: Vec<UsageRow> = parsed
        .iter()
        .map(|&(account, login, raw)| UsageRow {
            account: account.to_string(),
            login: login.to_string(),
            usage_raw: raw,
            usage_dollars: raw as f64 / divisor as f64,
        })
        .collect();

    let json = UsageJson {
        module: "usage",
        version: env!("CARGO_PKG_VERSION"),
        user: user.to_string(),
        cluster,
        year,
        month,
        start_date: start_date.to_string(),
        end_date: end_date.to_string(),
        billing_divisor: divisor,
        accounts,
    };
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sreport_output() {
        // Simulated sreport -nP output
        let output = "arc-ts|jdoe|12345678|\nother-acct|jdoe|500000|\n";
        let rows = parse_slurm_kv(output);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "arc-ts");
        assert_eq!(rows[0][2], "12345678");

        let parsed: Vec<(&str, &str, u64)> = rows
            .iter()
            .filter(|r| r.len() >= 3)
            .filter_map(|r| {
                let raw: u64 = r[2].trim().parse().ok()?;
                Some((r[0], r[1], raw))
            })
            .collect();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].2, 12345678);
    }

    #[test]
    fn usage_dollars_calculation() {
        let raw: u64 = 10_000_000;
        let divisor: u64 = 10_000_000;
        let dollars = raw as f64 / divisor as f64;
        assert!((dollars - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn old_divisor_calculation() {
        let raw: u64 = 100_000;
        let divisor: u64 = 100_000;
        let dollars = raw as f64 / divisor as f64;
        assert!((dollars - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_sreport_no_trailing_pipe() {
        let output = "testacct|testuser|3240113\notheracct|testuser|500000\n";
        let rows = parse_slurm_kv(output);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "testacct");
        assert_eq!(rows[0][2], "3240113");
    }

    #[test]
    fn parse_sreport_empty_output() {
        let output = "";
        let rows = parse_slurm_kv(output);
        let parsed: Vec<(&str, &str, u64)> = rows
            .iter()
            .filter(|r| r.len() >= 3)
            .filter_map(|r| {
                let raw: u64 = r[2].trim().parse().ok()?;
                Some((r[0], r[1], raw))
            })
            .collect();
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_sreport_malformed_row_skipped() {
        // Row with only 2 fields + row with non-numeric usage
        let output = "acct|user\nacct2|user2|notanumber\nacct3|user3|999\n";
        let rows = parse_slurm_kv(output);
        let parsed: Vec<(&str, &str, u64)> = rows
            .iter()
            .filter(|r| r.len() >= 3)
            .filter_map(|r| {
                let raw: u64 = r[2].trim().parse().ok()?;
                Some((r[0], r[1], raw))
            })
            .collect();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "acct3");
        assert_eq!(parsed[0].2, 999);
    }
}
