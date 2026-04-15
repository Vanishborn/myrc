use std::env;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use serde::Serialize;

use crate::common::{
    Align, Column, OutputMode, Table, TerminalInfo, parse_slurm_kv, resolve_user, slurm_cmd,
};

/// Arguments for `myrc accounts`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// User to query (also accepted positionally for backward compat).
    #[arg(value_name = "USER")]
    pub user: Option<String>,

    /// User to query (flag form).
    #[arg(short = 'u', long = "user")]
    pub user_flag: Option<String>,
}

#[derive(Debug, Serialize)]
struct AccountsJson {
    module: &'static str,
    version: &'static str,
    user: String,
    cluster: Option<String>,
    accounts: Vec<AccountRow>,
}

#[derive(Debug, Serialize)]
struct AccountRow {
    cluster: String,
    account: String,
    grp_tres: String,
    grp_tres_mins: String,
    max_jobs: String,
    max_tres: String,
    max_submit: String,
    max_wall: String,
    qos: String,
}

const HEADERS: [&str; 9] = [
    "Cluster",
    "Account",
    "GrpTRES",
    "GrpTRESMins",
    "MaxJobs",
    "MaxTRES",
    "MaxSubmit",
    "MaxWall",
    "QOS",
];

const FORMAT_FIELDS: &str =
    "cluster,account%30,GrpTRES%30,GrpTRESMins,MaxJobs,MaxTRES,MaxSubmit,MaxWall,QOS";

pub async fn run(args: &Args, output_mode: OutputMode) -> Result<()> {
    let user = resolve_user(args.user.as_deref(), args.user_flag.as_deref())?;
    let cluster = env::var("CLUSTER_NAME").ok().filter(|s| !s.is_empty());

    // Build sacctmgr command
    let mut cmd_args = vec![
        "sacctmgr".to_string(),
        "-n".to_string(),
        "-p".to_string(),
        "list".to_string(),
        "assoc".to_string(),
    ];

    if let Some(ref c) = cluster {
        cmd_args.push(format!("cluster={c}"));
    }
    cmd_args.push(format!("user={user}"));
    cmd_args.push(format!("format={FORMAT_FIELDS}"));

    let output = slurm_cmd(&cmd_args).await.context("querying accounts")?;

    let rows = parse_slurm_kv(&output);

    if rows.is_empty() {
        if output_mode.is_json() {
            let json = AccountsJson {
                module: "accounts",
                version: env!("CARGO_PKG_VERSION"),
                user,
                cluster,
                accounts: vec![],
            };
            println!("{}", serde_json::to_string_pretty(&json)?);
        } else {
            eprintln!("No accounts found for user '{user}'.");
        }
        return Ok(());
    }

    if output_mode.is_json() {
        return print_json(&user, cluster, &rows);
    }

    let info = TerminalInfo::detect();
    if info.is_narrow() {
        print_narrow(&rows);
    } else {
        print_wide(&rows);
    }

    Ok(())
}

fn print_json(user: &str, cluster: Option<String>, rows: &[Vec<&str>]) -> Result<()> {
    let accounts: Vec<AccountRow> = rows
        .iter()
        .filter(|r| r.len() >= 9)
        .map(|r| AccountRow {
            cluster: r[0].to_string(),
            account: r[1].to_string(),
            grp_tres: r[2].to_string(),
            grp_tres_mins: r[3].to_string(),
            max_jobs: r[4].to_string(),
            max_tres: r[5].to_string(),
            max_submit: r[6].to_string(),
            max_wall: r[7].to_string(),
            qos: r[8].to_string(),
        })
        .collect();

    let json = AccountsJson {
        module: "accounts",
        version: env!("CARGO_PKG_VERSION"),
        user: user.to_string(),
        cluster,
        accounts,
    };
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

fn print_wide(rows: &[Vec<&str>]) {
    let mut table = Table::new(
        HEADERS
            .iter()
            .map(|h| Column {
                header: h.to_string(),
                align: Align::Left,
            })
            .collect(),
    );

    for row in rows {
        if row.len() >= 9 {
            table.add_row(row.iter().map(|s| s.to_string()).collect());
        }
    }
    print!("{table}");
}

fn print_narrow(rows: &[Vec<&str>]) {
    for row in rows {
        if row.len() < 9 {
            continue;
        }
        println!();
        println!("{{");
        for (i, header) in HEADERS.iter().enumerate() {
            if !row[i].is_empty() {
                println!("  {:<15} : \"{}\"", header, row[i]);
            }
        }
        println!("}}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_wide_formats_correctly() {
        let rows = vec![vec![
            "greatlakes",
            "arc-ts",
            "cpu=500",
            "billing=50000000",
            "1000",
            "cpu=100",
            "2000",
            "7-00:00:00",
            "normal",
        ]];
        // Should not panic
        print_wide(&rows);
    }

    #[test]
    fn print_narrow_formats_correctly() {
        let rows = vec![vec![
            "greatlakes",
            "arc-ts",
            "cpu=500",
            "billing=50000000",
            "1000",
            "cpu=100",
            "2000",
            "7-00:00:00",
            "normal",
        ]];
        print_narrow(&rows);
    }

    #[test]
    fn empty_rows_handled() {
        let rows: Vec<Vec<&str>> = vec![];
        print_wide(&rows);
        print_narrow(&rows);
    }
}
