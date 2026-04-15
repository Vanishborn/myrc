use anyhow::Result;
use clap::Args as ClapArgs;
use serde::Serialize;
use std::env;

use crate::common::OutputMode;

/// Print Slurm job environment header (for use in job scripts).
#[derive(Debug, ClapArgs)]
pub struct Args {}

const LINE: &str = "#-------------------------------------------------------------------";

/// Slurm environment variables to display (in order).
const SLURM_VARS: &[(&str, &str)] = &[
    ("SLURM_SUBMIT_HOST", "SLURM_SUBMIT_HOST   "),
    ("SLURM_JOB_ACCOUNT", "SLURM_JOB_ACCOUNT   "),
    ("SLURM_JOB_PARTITION", "SLURM_JOB_PARTITION "),
    ("SLURM_JOB_NAME", "SLURM_JOB_NAME      "),
    ("SLURM_JOBID", "SLURM_JOBID         "),
    ("SLURM_NODELIST", "SLURM_NODELIST      "),
    ("SLURM_JOB_NUM_NODES", "SLURM_JOB_NUM_NODES "),
    ("SLURM_NTASKS", "SLURM_NTASKS        "),
    ("SLURM_TASKS_PER_NODE", "SLURM_TASKS_PER_NODE"),
    ("SLURM_CPUS_PER_TASK", "SLURM_CPUS_PER_TASK "),
    ("SLURM_NPROCS", "SLURM_NPROCS        "),
    ("SLURM_MEM_PER_CPU", "SLURM_MEM_PER_CPU   "),
];

#[derive(Serialize)]
struct JobHeaderJson {
    module: &'static str,
    version: &'static str,
    env: std::collections::BTreeMap<String, String>,
    gpus: Option<Vec<String>>,
    hostname: String,
    timestamp: String,
    modules: Vec<String>,
    ulimits: std::collections::BTreeMap<String, serde_json::Value>,
}

pub fn run(_args: &Args, output_mode: OutputMode) -> Result<()> {
    if output_mode.is_json() {
        print_json()
    } else {
        print_header()
    }
}

fn print_header() -> Result<()> {
    println!();
    println!("Job information");
    println!("{LINE}");

    // Print Slurm env vars
    for &(var, label) in SLURM_VARS {
        let val = env::var(var).unwrap_or_default();
        println!("{label} {val}");
    }

    // GPU section
    if env::var("GPU_DEVICE_ORDINAL").is_ok() {
        let ordinal = env::var("GPU_DEVICE_ORDINAL").unwrap_or_default();
        println!("GPU_DEVICE_ORDINAL   {ordinal}");
        // Run nvidia-smi -L
        if let Ok(output) = std::process::Command::new("nvidia-smi").arg("-L").output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            print!("{stdout}");
        }
    }

    // SLURM_SUBMIT_DIR is printed after GPU section in legacy
    let submit_dir = env::var("SLURM_SUBMIT_DIR").unwrap_or_default();
    println!("SLURM_SUBMIT_DIR     {submit_dir}");

    println!();

    // ulimit -a, filtering out "unlimited" lines
    if let Ok(output) = std::process::Command::new("bash")
        .args(["-c", "ulimit -a"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if !line.contains("unlimited") {
                println!("{line}");
            }
        }
    }

    println!();

    // Running on hostname at date
    let hostname = hostname_str();
    let date = date_str();
    println!("Running on {hostname} at {date}");

    // Module list (drop last 2 header lines)
    if let Ok(output) = std::process::Command::new("bash")
        .args(["-c", "module --redirect list 2>&1"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        let count = lines.len();
        if count > 2 {
            for line in &lines[..count - 2] {
                println!("{line}");
            }
        } else {
            for line in &lines {
                println!("{line}");
            }
        }
    }

    println!("Your job output begins below the line");
    println!("{LINE}");

    Ok(())
}

fn print_json() -> Result<()> {
    // Collect all SLURM_* env vars
    let mut slurm_env = std::collections::BTreeMap::new();
    for (key, val) in env::vars() {
        if key.starts_with("SLURM_") {
            slurm_env.insert(key, val);
        }
    }

    // GPUs
    let gpus = if env::var("GPU_DEVICE_ORDINAL").is_ok() {
        if let Ok(output) = std::process::Command::new("nvidia-smi").arg("-L").output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            Some(
                stdout
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| l.to_string())
                    .collect(),
            )
        } else {
            Some(vec![])
        }
    } else {
        None
    };

    // Hostname + timestamp
    let hostname = hostname_str();
    let timestamp = chrono::Local::now().to_rfc3339();

    // Modules
    let modules = if let Ok(output) = std::process::Command::new("bash")
        .args(["-c", "module --redirect list 2>&1"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        let count = lines.len();
        let module_lines = if count > 2 {
            &lines[..count - 2]
        } else {
            &lines[..]
        };
        // Parse module names: each numbered line like "  1) gcc/13.2.0  2) openmpi/4.1.6"
        let mut mods = Vec::new();
        for line in module_lines {
            for part in line.split_whitespace() {
                // Skip the number) prefix
                if part.ends_with(')') && part.chars().all(|c| c.is_ascii_digit() || c == ')') {
                    continue;
                }
                if !part.is_empty() && part.contains('/') {
                    mods.push(part.to_string());
                }
            }
        }
        mods
    } else {
        vec![]
    };

    // Ulimits
    let mut ulimits = std::collections::BTreeMap::new();
    if let Ok(output) = std::process::Command::new("bash")
        .args(["-c", "ulimit -a"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            // Lines look like: "core file size          (blocks, -c) 0"
            // or:               "open files                      (-n) 1024"
            if let Some((label, rest)) = line.rsplit_once(')') {
                let label = label.split('(').next().unwrap_or(label).trim();
                let val = rest.trim();
                if val == "unlimited" {
                    ulimits.insert(
                        label.to_string(),
                        serde_json::Value::String("unlimited".to_string()),
                    );
                } else if let Ok(n) = val.parse::<u64>() {
                    ulimits.insert(label.to_string(), serde_json::json!(n));
                } else {
                    ulimits.insert(
                        label.to_string(),
                        serde_json::Value::String(val.to_string()),
                    );
                }
            }
        }
    }

    let json = JobHeaderJson {
        module: "job_header",
        version: env!("CARGO_PKG_VERSION"),
        env: slurm_env,
        gpus,
        hostname,
        timestamp,
        modules,
        ulimits,
    };

    println!("{}", serde_json::to_string_pretty(&json).unwrap());

    Ok(())
}

fn hostname_str() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn date_str() -> String {
    std::process::Command::new("date")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hostname_str_returns_something() {
        let h = hostname_str();
        assert!(!h.is_empty());
    }

    #[test]
    fn test_date_str_returns_something() {
        let d = date_str();
        assert!(!d.is_empty());
    }

    #[test]
    fn test_line_constant_format() {
        assert!(LINE.starts_with('#'));
        assert!(LINE.contains("---"));
    }
}
