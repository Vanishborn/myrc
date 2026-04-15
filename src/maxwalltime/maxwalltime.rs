use std::fs;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::US::Eastern;
use clap::Args as ClapArgs;
use colored::Colorize;

use crate::common::{
    ClusterEnv, OutputMode, color_warning, format_walltime_human, format_walltime_slurm,
};

/// Maximum walltime cap: 14 days.
const MAX_DAYS: u64 = 14;
const MAX_SECONDS: u64 = MAX_DAYS * 24 * 60 * 60;

/// Buffer time before maintenance: 6 hours.
const BUFFER_HOURS: u64 = 6;
const BUFFER_SECONDS: u64 = BUFFER_HOURS * 60 * 60;

/// Arguments for `myrc maxwalltime`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Print only the walltime in Slurm format DD-HH:MM:SS.
    #[arg(short = 'S', long = "slurm-format")]
    pub slurm_format: bool,

    /// Hidden backward-compat alias for --slurm-format.
    #[arg(short = 's', hide = true)]
    pub slurm_format_short: bool,
}

impl Args {
    /// Whether Slurm-format-only output was requested (either flag).
    fn slurm_format_requested(&self) -> bool {
        self.slurm_format || self.slurm_format_short
    }
}

pub fn run(args: &Args, mode: OutputMode) -> Result<()> {
    let cluster = ClusterEnv::from_env()?;
    let epoch_path = cluster.epoch_path();

    let epoch_str = fs::read_to_string(&epoch_path)
        .with_context(|| format!("reading {}", epoch_path.display()))?;
    let epoch: i64 = epoch_str
        .trim()
        .parse()
        .with_context(|| format!("parsing epoch from {}", epoch_path.display()))?;

    let now = Utc::now();
    let now_epoch = now.timestamp();

    // Determine if maintenance is scheduled and in the future
    let maintenance_scheduled = epoch != 0 && epoch > now_epoch;

    if !maintenance_scheduled {
        // No maintenance or past: use 14-day cap
        let max_wt = Duration::from_secs(MAX_SECONDS);
        let slurm_fmt = format_walltime_slurm(max_wt);

        if mode.is_json() {
            let output = serde_json::json!({
                "module": "maxwalltime",
                "version": env!("CARGO_PKG_VERSION"),
                "cluster": cluster.name,
                "maintenance_scheduled": false,
                "maintenance_time": null,
                "maintenance_epoch": null,
                "now_epoch": now_epoch,
                "remaining_seconds": MAX_SECONDS,
                "remaining_slurm": slurm_fmt,
                "capped_at_14_days": true,
                "within_6h_warning": false,
                "max_walltime_seconds": MAX_SECONDS,
                "max_walltime_slurm": slurm_fmt,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
            return Ok(());
        }

        if args.slurm_format_requested() {
            println!("{slurm_fmt}");
        } else {
            let human_fmt = format_walltime_human(max_wt);
            println!("No maintenance window currently scheduled.");
            println!("Maximum wall time is {slurm_fmt} ({human_fmt}).");
        }
        return Ok(());
    }

    // Maintenance is scheduled and in the future
    let outage_time: DateTime<Utc> = Utc
        .timestamp_opt(epoch, 0)
        .single()
        .context("invalid maintenance epoch timestamp")?;

    let remaining_to_outage = (epoch - now_epoch) as u64;
    let within_6h = remaining_to_outage <= BUFFER_SECONDS;

    // Effective walltime: subtract buffer, cap at 14 days
    let effective_seconds = if within_6h {
        0u64
    } else {
        let raw = remaining_to_outage.saturating_sub(BUFFER_SECONDS);
        raw.min(MAX_SECONDS)
    };

    let capped = remaining_to_outage.saturating_sub(BUFFER_SECONDS) > MAX_SECONDS;
    let max_wt = Duration::from_secs(effective_seconds);
    let slurm_fmt = format_walltime_slurm(max_wt);
    let human_fmt = format_walltime_human(max_wt);

    if mode.is_json() {
        let local_outage = outage_time.with_timezone(&Eastern);
        let output = serde_json::json!({
            "module": "maxwalltime",
            "version": env!("CARGO_PKG_VERSION"),
            "cluster": cluster.name,
            "maintenance_scheduled": true,
            "maintenance_time": local_outage.to_rfc3339(),
            "maintenance_epoch": epoch,
            "now_epoch": now_epoch,
            "remaining_seconds": effective_seconds,
            "remaining_slurm": slurm_fmt,
            "capped_at_14_days": capped,
            "within_6h_warning": within_6h,
            "max_walltime_seconds": effective_seconds,
            "max_walltime_slurm": slurm_fmt,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if args.slurm_format_requested() {
        println!("{slurm_fmt}");
        return Ok(());
    }

    // Human-readable output with colored warnings matching legacy
    let local_outage = outage_time.with_timezone(&Eastern);
    println!(
        "{}",
        local_outage.format("Maintenance window scheduled to start at %T (%Z) on %A %m/%d/%Y.")
    );

    if now >= outage_time {
        println!("Maintenance window in progress.");
    } else if within_6h {
        println!("{}", color_warning("Maintenance window imminent.").bold());
        println!(
			"{}",
			color_warning("Jobs that cannot finish prior to the maintenance should not be submitted until the maintenance is complete.")
				.bold()
		);
    } else {
        let days_until = remaining_to_outage / 86400;
        let day_word = if days_until < 1 { "day" } else { "days" };
        println!(
            "{}",
            color_warning(&format!(
                "Maintenance window begins in less than {} {day_word}.",
                days_until + 1
            ))
            .bold()
        );
        if effective_seconds < 86400 {
            println!(
                "{}",
                color_warning(&format!(
                    "Recommended maximum wall time for new jobs is {slurm_fmt}"
                ))
                .bold()
            );
        } else {
            println!(
                "{}",
                color_warning(&format!(
                    "Recommended maximum wall time for new jobs is {slurm_fmt} ({human_fmt})."
                ))
                .bold()
            );
        }
    }

    println!("Maximum wall time is {slurm_fmt} ({human_fmt}).");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_days_constant() {
        assert_eq!(MAX_DAYS, 14);
        assert_eq!(MAX_SECONDS, 14 * 24 * 3600);
    }

    #[test]
    fn buffer_constant() {
        assert_eq!(BUFFER_SECONDS, 6 * 3600);
    }

    #[test]
    fn slurm_format_flag_detection() {
        let args = Args {
            slurm_format: true,
            slurm_format_short: false,
        };
        assert!(args.slurm_format_requested());

        let args2 = Args {
            slurm_format: false,
            slurm_format_short: true,
        };
        assert!(args2.slurm_format_requested());

        let args3 = Args {
            slurm_format: false,
            slurm_format_short: false,
        };
        assert!(!args3.slurm_format_requested());
    }

    #[test]
    fn walltime_cap_14_days() {
        let max_wt = Duration::from_secs(MAX_SECONDS);
        let slurm = format_walltime_slurm(max_wt);
        assert_eq!(slurm, "14-00:00:00");
        let human = format_walltime_human(max_wt);
        assert_eq!(human, "336:00:00");
    }

    #[test]
    fn effective_seconds_computation() {
        // 20 days remaining → capped at 14 days after buffer subtraction
        let remaining: u64 = 20 * 86400;
        let raw = remaining.saturating_sub(BUFFER_SECONDS);
        let effective = raw.min(MAX_SECONDS);
        assert_eq!(effective, MAX_SECONDS);
    }

    #[test]
    fn effective_seconds_within_buffer() {
        // 5 hours remaining → within 6h buffer → 0
        let remaining: u64 = 5 * 3600;
        let within_6h = remaining <= BUFFER_SECONDS;
        assert!(within_6h);
    }

    #[test]
    fn effective_seconds_normal() {
        // 3 days remaining → 3 days minus 6 hours
        let remaining: u64 = 3 * 86400;
        let raw = remaining.saturating_sub(BUFFER_SECONDS);
        let effective = raw.min(MAX_SECONDS);
        assert_eq!(effective, 3 * 86400 - 6 * 3600);
        let wt = Duration::from_secs(effective);
        let slurm = format_walltime_slurm(wt);
        assert_eq!(slurm, "02-18:00:00");
    }

    #[test]
    fn eastern_timezone_conversion() {
        // Verify chrono-tz Eastern is accessible
        let dt = Utc.timestamp_opt(1745146800, 0).single().unwrap();
        let local = dt.with_timezone(&Eastern);
        // Just verify it doesn't panic and produces a valid string
        let s = local.to_rfc3339();
        assert!(!s.is_empty());
    }
}
