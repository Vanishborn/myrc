use anyhow::{Context, Result};
use clap::Args as ClapArgs;

use crate::common::{
    ClusterEnv, OutputMode, Table, color_error, color_success, color_warning, format_memory,
    slurm_cmd,
};

/// Arguments for `myrc sstate`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Filter to a specific partition.
    #[arg(short = 'p', long = "partition")]
    pub partition: Option<String>,

    /// Show raw availability (disable bottleneck rule and state filtering).
    #[arg(long = "raw")]
    pub raw: bool,
}

/// Parsed per-node resource data.
#[derive(Debug, Clone)]
struct NodeInfo {
    name: String,
    alloc_cpus: u64,
    total_cpus: u64,
    cpu_load: f64,
    alloc_mem_bytes: u64,
    total_mem_bytes: u64,
    alloc_gpus: u64,
    total_gpus: u64,
    has_gpus: bool,
    state: String,
}

impl NodeInfo {
    fn avail_cpus(&self) -> u64 {
        self.total_cpus.saturating_sub(self.alloc_cpus)
    }

    fn percent_used_cpu(&self) -> f64 {
        if self.total_cpus == 0 {
            0.0
        } else {
            (self.alloc_cpus as f64 / self.total_cpus as f64) * 100.0
        }
    }

    fn avail_mem_bytes(&self) -> u64 {
        self.total_mem_bytes.saturating_sub(self.alloc_mem_bytes)
    }

    fn percent_used_mem(&self) -> f64 {
        if self.total_mem_bytes == 0 {
            0.0
        } else {
            (self.alloc_mem_bytes as f64 / self.total_mem_bytes as f64) * 100.0
        }
    }

    fn avail_gpus(&self) -> u64 {
        self.total_gpus.saturating_sub(self.alloc_gpus)
    }

    fn percent_used_gpu(&self) -> f64 {
        if self.total_gpus == 0 {
            0.0
        } else {
            (self.alloc_gpus as f64 / self.total_gpus as f64) * 100.0
        }
    }

    /// Effective available CPUs after state filter + bottleneck rule.
    fn effective_avail_cpus(&self) -> u64 {
        if is_node_unavailable(&self.state) {
            return 0;
        }
        let cpu = self.avail_cpus();
        let mem = self.avail_mem_bytes();
        let gpu = self.avail_gpus();
        if cpu == 0 || mem == 0 || (self.has_gpus && gpu == 0) {
            0
        } else {
            cpu
        }
    }

    /// Effective available memory after state filter + bottleneck rule.
    fn effective_avail_mem_bytes(&self) -> u64 {
        if is_node_unavailable(&self.state) {
            return 0;
        }
        let cpu = self.avail_cpus();
        let mem = self.avail_mem_bytes();
        let gpu = self.avail_gpus();
        if cpu == 0 || mem == 0 || (self.has_gpus && gpu == 0) {
            0
        } else {
            mem
        }
    }

    /// Effective available GPUs after state filter + bottleneck rule.
    fn effective_avail_gpus(&self) -> u64 {
        if is_node_unavailable(&self.state) {
            return 0;
        }
        let cpu = self.avail_cpus();
        let mem = self.avail_mem_bytes();
        let gpu = self.avail_gpus();
        if cpu == 0 || mem == 0 || (self.has_gpus && gpu == 0) {
            0
        } else {
            gpu
        }
    }
}

pub async fn run(args: &Args, mode: OutputMode) -> Result<()> {
    let cluster = ClusterEnv::from_env()?;

    // If a partition is specified, validate it and get the node list
    let partition_nodes: Option<Vec<String>> = if let Some(part) = &args.partition {
        let output = slurm_cmd(&["scontrol", "show", "partition", part])
            .await
            .with_context(|| format!("partition '{part}' not found or scontrol failed"))?;
        let nodes = extract_partition_nodes(&output);
        if nodes.is_empty() {
            anyhow::bail!("partition '{part}' has no nodes");
        }
        Some(nodes)
    } else {
        None
    };

    // Query all nodes in one call
    let node_output = slurm_cmd(&["scontrol", "show", "node", "-o"])
        .await
        .context("querying node information")?;

    let all_nodes = parse_node_output(&node_output);

    // Filter to partition nodes if specified
    let nodes: Vec<NodeInfo> = if let Some(pnodes) = &partition_nodes {
        all_nodes
            .into_iter()
            .filter(|n| pnodes.contains(&n.name))
            .collect()
    } else {
        all_nodes
    };

    if nodes.is_empty() {
        if let Some(part) = &args.partition {
            println!("No nodes found for partition '{part}'.");
        } else {
            println!("No nodes found.");
        }
        return Ok(());
    }

    // Determine if any node in the set has GPUs
    let any_gpus = nodes.iter().any(|n| n.has_gpus);

    if mode.is_json() {
        print_json(&cluster, args, &nodes, any_gpus, args.raw)?;
    } else {
        print_table(&nodes, any_gpus, args.raw);
    }

    Ok(())
}

/// Extract the node list from `scontrol show partition` output.
///
/// Looks for a `Nodes=` field in the key-value output.
fn extract_partition_nodes(output: &str) -> Vec<String> {
    for line in output.lines() {
        for field in line.split_whitespace() {
            if let Some(node_expr) = field.strip_prefix("Nodes=") {
                return expand_node_list(node_expr);
            }
        }
    }
    Vec::new()
}

/// Expand a Slurm node list expression like `gl[3009-3012,3014]` into
/// individual node names.
///
/// Handles simple forms: `gl[3009-3012]`, `gl3009,gl3010`, `gl[3009-3012,3014-3016]`.
fn expand_node_list(expr: &str) -> Vec<String> {
    let mut result = Vec::new();
    // Split on comma-separated node specs, but be careful about brackets
    // Simple approach: split by comma outside brackets
    for part in split_node_expr(expr) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(bracket_start) = part.find('[') {
            let prefix = &part[..bracket_start];
            let rest = &part[bracket_start + 1..];
            let rest = rest.trim_end_matches(']');
            for range_part in rest.split(',') {
                if let Some((start_s, end_s)) = range_part.split_once('-') {
                    let start: u64 = match start_s.parse() {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let end: u64 = match end_s.parse() {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let width = start_s.len();
                    for i in start..=end {
                        result.push(format!("{prefix}{i:0>width$}"));
                    }
                } else if let Ok(num) = range_part.parse::<u64>() {
                    let width = range_part.len();
                    result.push(format!("{prefix}{num:0>width$}"));
                }
            }
        } else {
            result.push(part.to_string());
        }
    }
    result
}

/// Split a node expression by commas that are NOT inside brackets.
fn split_node_expr(expr: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0u32;
    for ch in expr.chars() {
        match ch {
            '[' => {
                depth += 1;
                current.push(ch);
            }
            ']' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Parse `scontrol show node -o` output into `NodeInfo` records.
fn parse_node_output(output: &str) -> Vec<NodeInfo> {
    let mut nodes = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(node) = parse_single_node(line) {
            nodes.push(node);
        }
    }
    nodes
}

/// Parse a single line of `scontrol show node -o` into a `NodeInfo`.
///
/// scontrol -o puts all fields on one line separated by spaces:
/// `NodeName=gl3009 Arch=x86_64 CoresPerSocket=18 CPUAlloc=36 CPUTot=36
///  CPULoad=5.80 ... AllocMem=55091 ... RealMemory=184320 ... Gres=gpu:2
///  ... GresUsed=gpu:v100:2(IDX:0-1) ... State=ALLOCATED ...`
fn parse_single_node(line: &str) -> Option<NodeInfo> {
    let get = |key: &str| -> Option<&str> {
        for field in line.split_whitespace() {
            if let Some(val) = field.strip_prefix(key) {
                if let Some(val) = val.strip_prefix('=') {
                    return Some(val);
                }
            }
        }
        None
    };

    let name = get("NodeName")?.to_string();
    let alloc_cpus: u64 = get("CPUAlloc").and_then(|v| v.parse().ok()).unwrap_or(0);
    let total_cpus: u64 = get("CPUTot").and_then(|v| v.parse().ok()).unwrap_or(0);
    let cpu_load: f64 = get("CPULoad").and_then(|v| v.parse().ok()).unwrap_or(0.0);

    // Memory: scontrol reports AllocMem and RealMemory in MB
    let alloc_mem_mb: u64 = get("AllocMem").and_then(|v| v.parse().ok()).unwrap_or(0);
    let total_mem_mb: u64 = get("RealMemory").and_then(|v| v.parse().ok()).unwrap_or(0);
    let alloc_mem_bytes = alloc_mem_mb * (1 << 20);
    let total_mem_bytes = total_mem_mb * (1 << 20);

    let state = get("State").unwrap_or("UNKNOWN").to_string();

    // GPU parsing: Gres for total, GresUsed or AllocTRES for allocated.
    // Slurm 25+ may omit GresUsed; GPU allocation is in AllocTRES instead.
    let (total_gpus, alloc_gpus, has_gpus) = parse_gpu_fields(
        get("Gres").unwrap_or(""),
        get("GresUsed").unwrap_or(""),
        get("AllocTRES").unwrap_or(""),
    );

    Some(NodeInfo {
        name,
        alloc_cpus,
        total_cpus,
        cpu_load,
        alloc_mem_bytes,
        total_mem_bytes,
        alloc_gpus,
        total_gpus,
        has_gpus,
        state,
    })
}

/// Parse GPU count from Gres, GresUsed, and AllocTRES fields.
///
/// `Gres` examples: `gpu:2`, `gpu:v100:2`, `gpu:a100:3`, `(null)`
/// `GresUsed` examples: `gpu:v100:2(IDX:0-1)`, `gpu:0`, `gpu:v100:0`
/// `AllocTRES` example: `cpu=9,mem=80G,gres/gpu=2`
///
/// Slurm 25+ may omit `GresUsed`; fall back to `AllocTRES` for allocated count.
fn parse_gpu_fields(gres: &str, gres_used: &str, alloc_tres: &str) -> (u64, u64, bool) {
    let total = parse_gpu_count(gres);
    let alloc = {
        let from_gres_used = parse_gpu_count(gres_used);
        if from_gres_used > 0 {
            from_gres_used
        } else {
            parse_alloc_tres_gpu(alloc_tres)
        }
    };
    let has = total > 0;
    (total, alloc, has)
}

/// Extract GPU count from a Gres-like string.
///
/// Handles: `gpu:2`, `gpu:v100:2`, `gpu:v100:2(IDX:0-1)`, `gpu:0`, `(null)`, empty.
fn parse_gpu_count(s: &str) -> u64 {
    if s.is_empty() || s == "(null)" {
        return 0;
    }
    // May have multiple GRES types separated by commas
    for part in s.split(',') {
        let part = part.trim();
        if !part.starts_with("gpu") {
            continue;
        }
        // Strip (IDX:...) suffix if present
        let clean = if let Some(idx) = part.find('(') {
            &part[..idx]
        } else {
            part
        };
        // Format: gpu:COUNT or gpu:MODEL:COUNT
        let segments: Vec<&str> = clean.split(':').collect();
        if let Some(last) = segments.last() {
            if let Ok(n) = last.parse::<u64>() {
                return n;
            }
        }
    }
    0
}

/// Extract GPU count from an AllocTRES string.
///
/// Format: `cpu=9,mem=80G,gres/gpu=2`
/// Returns the value after `gres/gpu=`, or 0 if not present.
fn parse_alloc_tres_gpu(s: &str) -> u64 {
    for part in s.split(',') {
        if let Some(val) = part.strip_prefix("gres/gpu=") {
            return val.parse().unwrap_or(0);
        }
    }
    0
}

fn print_table(nodes: &[NodeInfo], any_gpus: bool, raw: bool) {
    let headers: Vec<&str> = vec![
        "Node",
        "AllocCPU",
        "AvailCPU",
        "TotalCPU",
        "PercentUsedCPU",
        "CPULoad",
        "AllocMem",
        "AvailMem",
        "TotalMem",
        "PercentUsedMem",
        "AllocGPU",
        "AvailGPU",
        "TotalGPU",
        "PercentUsedGPU",
        "NodeState",
    ];

    let mut table = Table::from_headers(&headers);
    // Right-align numeric columns
    for i in 1..14 {
        table.right_align(i);
    }

    for n in nodes {
        let avail_cpu = if raw {
            n.avail_cpus()
        } else {
            n.effective_avail_cpus()
        };
        let avail_mem = if raw {
            n.avail_mem_bytes()
        } else {
            n.effective_avail_mem_bytes()
        };

        let (gpu_alloc, gpu_avail, gpu_total, gpu_pct) = if n.has_gpus {
            let avail = if raw {
                n.avail_gpus()
            } else {
                n.effective_avail_gpus()
            };
            (
                n.alloc_gpus.to_string(),
                avail.to_string(),
                n.total_gpus.to_string(),
                format!("{:.2}", n.percent_used_gpu()),
            )
        } else {
            ("N/A".into(), "N/A".into(), "N/A".into(), "N/A".into())
        };

        table.add_row(vec![
            n.name.clone(),
            n.alloc_cpus.to_string(),
            avail_cpu.to_string(),
            n.total_cpus.to_string(),
            format!("{:.2}", n.percent_used_cpu()),
            format!("{:.2}", n.cpu_load),
            format_memory(n.alloc_mem_bytes),
            format_memory(avail_mem),
            format_memory(n.total_mem_bytes),
            format!("{:.2}", n.percent_used_mem()),
            gpu_alloc,
            gpu_avail,
            gpu_total,
            gpu_pct,
            n.state.clone(),
        ]);
    }

    // Pre-compute owned color metadata for the 'static closure
    let node_color_info: Vec<(String, bool, bool)> = nodes
        .iter()
        .map(|n| {
            let unavailable = is_node_unavailable(&n.state);
            let bottleneck = !raw
                && !unavailable
                && n.effective_avail_cpus() == 0
                && n.effective_avail_mem_bytes() == 0;
            (n.state.clone(), unavailable, bottleneck)
        })
        .collect();

    table.set_cell_color(move |row_idx, col_idx, padded| {
        let (ref state, unavailable, bottleneck) = node_color_info[row_idx];

        // NodeState column (14): color by state flags
        if col_idx == 14 {
            if unavailable {
                if state
                    .split('+')
                    .any(|f| f == "DOWN" || f == "NOT_RESPONDING")
                {
                    return color_error(padded).to_string();
                }
                return color_warning(padded).to_string();
            }
            if state == "IDLE" {
                return color_success(padded).to_string();
            }
            return padded.to_string();
        }

        // Avail columns (2=AvailCPU, 7=AvailMem, 11=AvailGPU):
        // red if bottleneck (all effective=0) on a normally-available node
        if bottleneck && matches!(col_idx, 2 | 7 | 11) {
            return color_error(padded).to_string();
        }

        padded.to_string()
    });

    // Totals row
    let node_count = nodes.len() as u64;
    let total_alloc_cpus: u64 = nodes.iter().map(|n| n.alloc_cpus).sum();
    let total_avail_cpus: u64 = nodes
        .iter()
        .map(|n| {
            if raw {
                n.avail_cpus()
            } else {
                n.effective_avail_cpus()
            }
        })
        .sum();
    let total_cpus: u64 = nodes.iter().map(|n| n.total_cpus).sum();
    let total_cpu_load: f64 = nodes.iter().map(|n| n.cpu_load).sum::<f64>() / nodes.len() as f64;
    let total_alloc_mem: u64 = nodes.iter().map(|n| n.alloc_mem_bytes).sum();
    let total_avail_mem: u64 = nodes
        .iter()
        .map(|n| {
            if raw {
                n.avail_mem_bytes()
            } else {
                n.effective_avail_mem_bytes()
            }
        })
        .sum();
    let total_mem: u64 = nodes.iter().map(|n| n.total_mem_bytes).sum();
    let pct_cpu = if total_cpus > 0 {
        (total_alloc_cpus as f64 / total_cpus as f64) * 100.0
    } else {
        0.0
    };
    let pct_mem = if total_mem > 0 {
        (total_alloc_mem as f64 / total_mem as f64) * 100.0
    } else {
        0.0
    };

    let total_alloc_gpus: u64 = nodes.iter().map(|n| n.alloc_gpus).sum();
    let total_avail_gpus: u64 = nodes
        .iter()
        .map(|n| {
            if raw {
                n.avail_gpus()
            } else {
                n.effective_avail_gpus()
            }
        })
        .sum();
    let total_gpus_sum: u64 = nodes.iter().map(|n| n.total_gpus).sum();
    let pct_gpu = if total_gpus_sum > 0 {
        (total_alloc_gpus as f64 / total_gpus_sum as f64) * 100.0
    } else {
        0.0
    };

    let gpu_alloc_total = if any_gpus {
        total_alloc_gpus.to_string()
    } else {
        "N/A".into()
    };
    let gpu_avail_total = if any_gpus {
        total_avail_gpus.to_string()
    } else {
        "N/A".into()
    };
    let gpu_total_total = if any_gpus {
        total_gpus_sum.to_string()
    } else {
        "N/A".into()
    };
    let gpu_pct_total = if any_gpus {
        format!("{:.2}", pct_gpu)
    } else {
        "N/A".into()
    };

    table.set_totals(vec![
        node_count.to_string(),
        total_alloc_cpus.to_string(),
        total_avail_cpus.to_string(),
        total_cpus.to_string(),
        format!("{:.2}", pct_cpu),
        format!("{:.2}", total_cpu_load),
        format_memory(total_alloc_mem),
        format_memory(total_avail_mem),
        format_memory(total_mem),
        format!("{:.2}", pct_mem),
        gpu_alloc_total,
        gpu_avail_total,
        gpu_total_total,
        gpu_pct_total,
        String::new(),
    ]);

    print!("{table}");
}

fn print_json(
    cluster: &ClusterEnv,
    args: &Args,
    nodes: &[NodeInfo],
    any_gpus: bool,
    raw: bool,
) -> Result<()> {
    let json_nodes: Vec<serde_json::Value> = nodes
        .iter()
        .map(|n| {
            let avail_cpu = if raw {
                n.avail_cpus()
            } else {
                n.effective_avail_cpus()
            };
            let avail_mem = if raw {
                n.avail_mem_bytes()
            } else {
                n.effective_avail_mem_bytes()
            };
            let mut obj = serde_json::json!({
                "node": n.name,
                "alloc_cpus": n.alloc_cpus,
                "avail_cpus": avail_cpu,
                "total_cpus": n.total_cpus,
                "percent_used_cpu": round2(n.percent_used_cpu()),
                "cpu_load": round2(n.cpu_load),
                "alloc_mem_bytes": n.alloc_mem_bytes,
                "avail_mem_bytes": avail_mem,
                "total_mem_bytes": n.total_mem_bytes,
                "percent_used_mem": round2(n.percent_used_mem()),
                "state": n.state,
            });
            if n.has_gpus {
                let avail_gpu = if raw {
                    n.avail_gpus()
                } else {
                    n.effective_avail_gpus()
                };
                obj["alloc_gpus"] = serde_json::json!(n.alloc_gpus);
                obj["avail_gpus"] = serde_json::json!(avail_gpu);
                obj["total_gpus"] = serde_json::json!(n.total_gpus);
                obj["percent_used_gpu"] = serde_json::json!(round2(n.percent_used_gpu()));
            } else {
                obj["alloc_gpus"] = serde_json::Value::Null;
                obj["avail_gpus"] = serde_json::Value::Null;
                obj["total_gpus"] = serde_json::Value::Null;
                obj["percent_used_gpu"] = serde_json::Value::Null;
            }
            obj
        })
        .collect();

    // Compute totals
    let node_count = nodes.len() as u64;
    let total_alloc_cpus: u64 = nodes.iter().map(|n| n.alloc_cpus).sum();
    let total_avail_cpus: u64 = nodes
        .iter()
        .map(|n| {
            if raw {
                n.avail_cpus()
            } else {
                n.effective_avail_cpus()
            }
        })
        .sum();
    let total_cpus: u64 = nodes.iter().map(|n| n.total_cpus).sum();
    let total_cpu_load: f64 =
        nodes.iter().map(|n| n.cpu_load).sum::<f64>() / nodes.len().max(1) as f64;
    let total_alloc_mem: u64 = nodes.iter().map(|n| n.alloc_mem_bytes).sum();
    let total_avail_mem: u64 = nodes
        .iter()
        .map(|n| {
            if raw {
                n.avail_mem_bytes()
            } else {
                n.effective_avail_mem_bytes()
            }
        })
        .sum();
    let total_mem: u64 = nodes.iter().map(|n| n.total_mem_bytes).sum();
    let pct_cpu = if total_cpus > 0 {
        (total_alloc_cpus as f64 / total_cpus as f64) * 100.0
    } else {
        0.0
    };
    let pct_mem = if total_mem > 0 {
        (total_alloc_mem as f64 / total_mem as f64) * 100.0
    } else {
        0.0
    };

    let total_alloc_gpus: u64 = nodes.iter().map(|n| n.alloc_gpus).sum();
    let total_avail_gpus: u64 = nodes
        .iter()
        .map(|n| {
            if raw {
                n.avail_gpus()
            } else {
                n.effective_avail_gpus()
            }
        })
        .sum();
    let total_gpus_sum: u64 = nodes.iter().map(|n| n.total_gpus).sum();
    let pct_gpu = if total_gpus_sum > 0 {
        (total_alloc_gpus as f64 / total_gpus_sum as f64) * 100.0
    } else {
        0.0
    };

    let mut totals = serde_json::json!({
        "node_count": node_count,
        "alloc_cpus": total_alloc_cpus,
        "avail_cpus": total_avail_cpus,
        "total_cpus": total_cpus,
        "percent_used_cpu": round2(pct_cpu),
        "cpu_load": round2(total_cpu_load),
        "alloc_mem_bytes": total_alloc_mem,
        "avail_mem_bytes": total_avail_mem,
        "total_mem_bytes": total_mem,
        "percent_used_mem": round2(pct_mem),
    });

    if any_gpus {
        totals["alloc_gpus"] = serde_json::json!(total_alloc_gpus);
        totals["avail_gpus"] = serde_json::json!(total_avail_gpus);
        totals["total_gpus"] = serde_json::json!(total_gpus_sum);
        totals["percent_used_gpu"] = serde_json::json!(round2(pct_gpu));
    }

    let output = serde_json::json!({
        "module": "sstate",
        "version": env!("CARGO_PKG_VERSION"),
        "cluster": cluster.name,
        "partition": args.partition,
        "nodes": json_nodes,
        "totals": totals,
    });

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// Round to 2 decimal places.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Check if a node's state flags indicate it cannot accept new jobs.
fn is_node_unavailable(state: &str) -> bool {
    const UNAVAILABLE_FLAGS: &[&str] = &[
        "DOWN",
        "DRAIN",
        "NOT_RESPONDING",
        "MAINTENANCE",
        "RESERVED",
        "FUTURE",
        "POWER_DOWN",
        "POWERED_DOWN",
        "REBOOT_REQUESTED",
        "REBOOT_ISSUED",
        "INVALID_REG",
    ];
    state
        .split('+')
        .any(|flag| UNAVAILABLE_FLAGS.contains(&flag))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gpu_count_standard() {
        assert_eq!(parse_gpu_count("gpu:2"), 2);
        assert_eq!(parse_gpu_count("gpu:v100:2"), 2);
        assert_eq!(parse_gpu_count("gpu:v100:2(IDX:0-1)"), 2);
        assert_eq!(parse_gpu_count("gpu:0"), 0);
        assert_eq!(parse_gpu_count("(null)"), 0);
        assert_eq!(parse_gpu_count(""), 0);
    }

    #[test]
    fn parse_gpu_count_a100() {
        assert_eq!(parse_gpu_count("gpu:a100:3"), 3);
        assert_eq!(parse_gpu_count("gpu:a100:3(IDX:0-2)"), 3);
    }

    #[test]
    fn parse_alloc_tres_gpu_present() {
        assert_eq!(parse_alloc_tres_gpu("cpu=9,mem=80G,gres/gpu=2"), 2);
        assert_eq!(
            parse_alloc_tres_gpu("cpu=18,mem=106G,billing=54782,gres/gpu=3"),
            3
        );
    }

    #[test]
    fn parse_alloc_tres_gpu_absent() {
        assert_eq!(parse_alloc_tres_gpu("cpu=36,mem=180G"), 0);
        assert_eq!(parse_alloc_tres_gpu(""), 0);
    }

    #[test]
    fn parse_gpu_fields_alloc_tres_fallback() {
        // Slurm 25+: GresUsed absent, allocation in AllocTRES
        let (total, alloc, has) =
            parse_gpu_fields("gpu:v100:2(S:0-1)", "", "cpu=9,mem=80G,gres/gpu=2");
        assert_eq!(total, 2);
        assert_eq!(alloc, 2);
        assert!(has);
    }

    #[test]
    fn parse_gpu_fields_gres_used_preferred() {
        // When GresUsed is present, it takes priority
        let (total, alloc, has) = parse_gpu_fields(
            "gpu:v100:2",
            "gpu:v100:1(IDX:0)",
            "cpu=9,mem=80G,gres/gpu=1",
        );
        assert_eq!(total, 2);
        assert_eq!(alloc, 1);
        assert!(has);
    }

    #[test]
    fn expand_node_list_range() {
        let nodes = expand_node_list("gl[3009-3012]");
        assert_eq!(nodes, vec!["gl3009", "gl3010", "gl3011", "gl3012"]);
    }

    #[test]
    fn expand_node_list_commas_in_brackets() {
        let nodes = expand_node_list("gl[3009-3010,3014]");
        assert_eq!(nodes, vec!["gl3009", "gl3010", "gl3014"]);
    }

    #[test]
    fn expand_node_list_plain() {
        let nodes = expand_node_list("gl3009");
        assert_eq!(nodes, vec!["gl3009"]);
    }

    #[test]
    fn parse_single_node_basic() {
        let line = "NodeName=gl3009 Arch=x86_64 CoresPerSocket=18 \
					 CPUAlloc=36 CPUTot=36 CPULoad=5.80 \
					 AllocMem=55091 RealMemory=184320 \
					 Gres=gpu:v100:2 GresUsed=gpu:v100:1(IDX:0) \
					 State=MIXED";
        let node = parse_single_node(line).unwrap();
        assert_eq!(node.name, "gl3009");
        assert_eq!(node.alloc_cpus, 36);
        assert_eq!(node.total_cpus, 36);
        assert!((node.cpu_load - 5.80).abs() < 0.01);
        assert_eq!(node.alloc_mem_bytes, 55091 * (1 << 20));
        assert_eq!(node.total_mem_bytes, 184320 * (1 << 20));
        assert_eq!(node.total_gpus, 2);
        assert_eq!(node.alloc_gpus, 1);
        assert!(node.has_gpus);
        assert_eq!(node.state, "MIXED");
    }

    #[test]
    fn parse_single_node_slurm25_alloc_tres() {
        // Slurm 25+: no GresUsed, GPU allocation in AllocTRES
        let line = "NodeName=gl1000 Arch=x86_64 CPUAlloc=9 CPUTot=40 \
					 CPULoad=0.82 AllocMem=81920 RealMemory=184320 \
					 Gres=gpu:v100:2(S:0-1) \
					 CfgTRES=cpu=40,mem=180G,billing=54782,gres/gpu=2 \
					 AllocTRES=cpu=9,mem=80G,gres/gpu=2 \
					 State=MIXED+PLANNED";
        let node = parse_single_node(line).unwrap();
        assert_eq!(node.name, "gl1000");
        assert_eq!(node.alloc_cpus, 9);
        assert_eq!(node.total_cpus, 40);
        assert_eq!(node.total_gpus, 2);
        assert_eq!(node.alloc_gpus, 2);
        assert!(node.has_gpus);
        assert_eq!(node.state, "MIXED+PLANNED");
    }

    #[test]
    fn parse_single_node_no_gpu() {
        let line = "NodeName=gl3063 Arch=x86_64 CPUAlloc=0 CPUTot=36 \
					 CPULoad=0.09 AllocMem=0 RealMemory=184320 \
					 Gres=(null) GresUsed=gpu:0 State=IDLE+MAINTENANCE+RESERVED";
        let node = parse_single_node(line).unwrap();
        assert_eq!(node.name, "gl3063");
        assert_eq!(node.total_gpus, 0);
        assert_eq!(node.alloc_gpus, 0);
        assert!(!node.has_gpus);
        assert_eq!(node.state, "IDLE+MAINTENANCE+RESERVED");
    }

    #[test]
    fn node_info_derived_columns() {
        let node = NodeInfo {
            name: "test".into(),
            alloc_cpus: 20,
            total_cpus: 40,
            cpu_load: 5.0,
            alloc_mem_bytes: 80 * (1 << 30),
            total_mem_bytes: 180 * (1 << 30),
            alloc_gpus: 1,
            total_gpus: 2,
            has_gpus: true,
            state: "MIXED".into(),
        };
        assert_eq!(node.avail_cpus(), 20);
        assert!((node.percent_used_cpu() - 50.0).abs() < 0.01);
        assert_eq!(node.avail_mem_bytes(), 100 * (1 << 30));
        assert!((node.percent_used_mem() - 44.44).abs() < 0.01);
        assert_eq!(node.avail_gpus(), 1);
        assert!((node.percent_used_gpu() - 50.0).abs() < 0.01);
    }

    #[test]
    fn round2_test() {
        assert!((round2(28.023456) - 28.02).abs() < 1e-9);
        assert!((round2(0.005) - 0.01).abs() < 1e-9);
        assert!((round2(100.0) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn split_node_expr_basic() {
        let parts = split_node_expr("gl[3009-3012],gl[1000-1002]");
        assert_eq!(parts, vec!["gl[3009-3012]", "gl[1000-1002]"]);
    }

    #[test]
    fn expand_multiple_ranges() {
        let nodes = expand_node_list("gl[1000-1001]");
        assert_eq!(nodes, vec!["gl1000", "gl1001"]);
    }

    #[test]
    fn test_is_node_unavailable() {
        // Available states
        assert!(!is_node_unavailable("IDLE"));
        assert!(!is_node_unavailable("MIXED"));
        assert!(!is_node_unavailable("ALLOCATED"));
        assert!(!is_node_unavailable("MIXED+PLANNED"));
        assert!(!is_node_unavailable("COMPLETING"));
        assert!(!is_node_unavailable("ALLOCATED+COMPLETING"));

        // Unavailable states
        assert!(is_node_unavailable("DOWN+NOT_RESPONDING"));
        assert!(is_node_unavailable("IDLE+MAINTENANCE+RESERVED"));
        assert!(is_node_unavailable("MIXED+DRAIN"));
        assert!(is_node_unavailable("ALLOCATED+DRAIN"));
        assert!(is_node_unavailable("IDLE+RESERVED"));
        assert!(is_node_unavailable("ALLOCATED+RESERVED"));
        assert!(is_node_unavailable("MIXED+RESERVED"));
        assert!(is_node_unavailable(
            "DOWN+MAINTENANCE+RESERVED+NOT_RESPONDING"
        ));
        assert!(is_node_unavailable("DOWN+INVALID_REG"));
        assert!(is_node_unavailable("FUTURE"));
        assert!(is_node_unavailable("POWER_DOWN"));
        assert!(is_node_unavailable("REBOOT_ISSUED"));
    }

    #[test]
    fn test_effective_avail_bottleneck_gpu() {
        let node = NodeInfo {
            name: "gl1000".into(),
            alloc_cpus: 2,
            total_cpus: 40,
            cpu_load: 0.82,
            alloc_mem_bytes: 80 * (1 << 30),
            total_mem_bytes: 180 * (1 << 30),
            alloc_gpus: 2,
            total_gpus: 2,
            has_gpus: true,
            state: "MIXED+PLANNED".into(),
        };
        assert_eq!(node.avail_cpus(), 38);
        assert_eq!(node.effective_avail_cpus(), 0);
        assert_eq!(node.effective_avail_mem_bytes(), 0);
        assert_eq!(node.effective_avail_gpus(), 0);
    }

    #[test]
    fn test_effective_avail_bottleneck_mem() {
        let node = NodeInfo {
            name: "gl3074".into(),
            alloc_cpus: 4,
            total_cpus: 36,
            cpu_load: 1.0,
            alloc_mem_bytes: 180 * (1 << 30),
            total_mem_bytes: 180 * (1 << 30),
            alloc_gpus: 0,
            total_gpus: 0,
            has_gpus: false,
            state: "MIXED".into(),
        };
        assert_eq!(node.avail_cpus(), 32);
        assert_eq!(node.effective_avail_cpus(), 0);
        assert_eq!(node.effective_avail_mem_bytes(), 0);
    }

    #[test]
    fn test_effective_avail_bottleneck_cpu() {
        let node = NodeInfo {
            name: "gl3009".into(),
            alloc_cpus: 36,
            total_cpus: 36,
            cpu_load: 36.0,
            alloc_mem_bytes: 54 * (1 << 30),
            total_mem_bytes: 180 * (1 << 30),
            alloc_gpus: 0,
            total_gpus: 0,
            has_gpus: false,
            state: "ALLOCATED".into(),
        };
        assert_eq!(node.avail_mem_bytes(), 126 * (1 << 30));
        assert_eq!(node.effective_avail_cpus(), 0);
        assert_eq!(node.effective_avail_mem_bytes(), 0);
    }

    #[test]
    fn test_effective_avail_no_bottleneck() {
        let node = NodeInfo {
            name: "gl3010".into(),
            alloc_cpus: 29,
            total_cpus: 36,
            cpu_load: 29.0,
            alloc_mem_bytes: 175 * (1 << 30),
            total_mem_bytes: 180 * (1 << 30),
            alloc_gpus: 0,
            total_gpus: 0,
            has_gpus: false,
            state: "MIXED".into(),
        };
        assert_eq!(node.effective_avail_cpus(), 7);
        assert_eq!(node.effective_avail_mem_bytes(), 5 * (1 << 30));
    }

    #[test]
    fn test_effective_avail_unavailable_state() {
        let node = NodeInfo {
            name: "gl3399".into(),
            alloc_cpus: 24,
            total_cpus: 36,
            cpu_load: 24.0,
            alloc_mem_bytes: 168 * (1 << 30),
            total_mem_bytes: 180 * (1 << 30),
            alloc_gpus: 0,
            total_gpus: 0,
            has_gpus: false,
            state: "MIXED+DRAIN".into(),
        };
        assert_eq!(node.avail_cpus(), 12);
        assert_eq!(node.effective_avail_cpus(), 0);
        assert_eq!(node.effective_avail_mem_bytes(), 0);
    }

    #[test]
    fn test_effective_avail_idle_gpu_node() {
        let node = NodeInfo {
            name: "gl1018".into(),
            alloc_cpus: 0,
            total_cpus: 40,
            cpu_load: 0.0,
            alloc_mem_bytes: 0,
            total_mem_bytes: 180 * (1 << 30),
            alloc_gpus: 0,
            total_gpus: 2,
            has_gpus: true,
            state: "IDLE".into(),
        };
        assert_eq!(node.effective_avail_cpus(), 40);
        assert_eq!(node.effective_avail_mem_bytes(), 180 * (1 << 30));
        assert_eq!(node.effective_avail_gpus(), 2);
    }

    #[test]
    fn extract_partition_nodes_from_real_output() {
        let output = "\
PartitionName=gpu
   AllowGroups=ALL DenyAccounts=acct1,acct2 AllowQos=ALL
   Nodes=gl[1000-1023]
   State=UP TotalCPUs=960 TotalNodes=24
   TRESBillingWeights=cpu=1369.560185,mem=304.3467078G,GRES/gpu=27391.2037
";
        let nodes = extract_partition_nodes(output);
        assert_eq!(nodes.len(), 24);
        assert_eq!(nodes[0], "gl1000");
        assert_eq!(nodes[23], "gl1023");
    }

    #[test]
    fn extract_partition_nodes_armis2_style() {
        // Non-contiguous ranges
        let output = "\
PartitionName=gpu
   Nodes=armis[20108,20112-20114]
   State=UP TotalCPUs=88 TotalNodes=4
";
        let nodes = extract_partition_nodes(output);
        assert_eq!(nodes.len(), 4);
        assert_eq!(
            nodes,
            vec!["armis20108", "armis20112", "armis20113", "armis20114"]
        );
    }

    #[test]
    fn parse_gpu_count_model_titanv() {
        assert_eq!(parse_gpu_count("gpu:titanv:4"), 4);
        assert_eq!(parse_gpu_count("gpu:titanv:4(IDX:0-3)"), 4);
    }

    #[test]
    fn parse_single_node_allocated_no_gpu() {
        // Fully allocated CPU-only node, 1.5TB memory
        let line = "NodeName=gl0000 Arch=x86_64 CoresPerSocket=18 \
                     CPUAlloc=36 CPUTot=36 CPULoad=1.02 \
                     AllocMem=1280000 RealMemory=1539072 \
                     Gres=(null) GresUsed=gpu:0 \
                     AllocTRES=cpu=3,mem=1250G \
                     State=MIXED";
        let node = parse_single_node(line).unwrap();
        assert_eq!(node.name, "gl0000");
        assert_eq!(node.alloc_cpus, 36);
        assert_eq!(node.total_cpus, 36);
        assert_eq!(node.total_mem_bytes, 1539072 * (1 << 20));
        assert_eq!(node.total_gpus, 0);
        assert!(!node.has_gpus);
        assert_eq!(node.state, "MIXED");
    }

    #[test]
    fn parse_single_node_idle_non_gpu() {
        let line = "NodeName=gl3100 Arch=x86_64 \
                     CPUAlloc=0 CPUTot=36 CPULoad=0.00 \
                     AllocMem=0 RealMemory=184320 \
                     Gres=(null) GresUsed=gpu:0 \
                     AllocTRES= \
                     State=IDLE";
        let node = parse_single_node(line).unwrap();
        assert_eq!(node.alloc_cpus, 0);
        assert_eq!(node.total_cpus, 36);
        assert_eq!(node.alloc_mem_bytes, 0);
        assert_eq!(node.total_gpus, 0);
        assert!(!node.has_gpus);
        assert_eq!(node.state, "IDLE");
        assert_eq!(node.effective_avail_cpus(), 36);
        assert_eq!(node.effective_avail_mem_bytes(), 184320 * (1 << 20));
    }

    #[test]
    fn parse_single_node_allocated_state() {
        // ALLOCATED = all CPUs in use, non-GPU node
        let line = "NodeName=gl3050 Arch=x86_64 \
                     CPUAlloc=36 CPUTot=36 CPULoad=36.00 \
                     AllocMem=180000 RealMemory=184320 \
                     Gres=(null) GresUsed=gpu:0 \
                     State=ALLOCATED";
        let node = parse_single_node(line).unwrap();
        assert_eq!(node.state, "ALLOCATED");
        assert!(!is_node_unavailable(&node.state));
        // CPU bottleneck → effective = 0
        assert_eq!(node.effective_avail_cpus(), 0);
    }

    #[test]
    fn parse_alloc_tres_gpu_empty_string() {
        assert_eq!(parse_alloc_tres_gpu(""), 0);
    }

    #[test]
    fn parse_single_node_cpu_load_exceeds_cpu_tot() {
        // CPULoad > CPUTot (oversubscription)
        let line = "NodeName=gl0001 Arch=x86_64 \
                     CPUAlloc=14 CPUTot=36 CPULoad=55.92 \
                     AllocMem=1263360 RealMemory=1539072 \
                     Gres=(null) GresUsed=gpu:0 \
                     State=MIXED";
        let node = parse_single_node(line).unwrap();
        assert!((node.cpu_load - 55.92).abs() < 0.01);
        assert_eq!(node.total_cpus, 36);
        assert!(node.cpu_load > node.total_cpus as f64);
    }

    #[test]
    fn parse_node_output_multi_node() {
        let output = "\
NodeName=gl3009 CPUAlloc=29 CPUTot=36 CPULoad=29.00 AllocMem=175000 RealMemory=184320 Gres=(null) GresUsed=gpu:0 State=MIXED
NodeName=gl1000 CPUAlloc=9 CPUTot=40 CPULoad=0.82 AllocMem=81920 RealMemory=184320 Gres=gpu:v100:2(S:0-1) AllocTRES=cpu=9,mem=80G,gres/gpu=2 State=MIXED+PLANNED";
        let nodes = parse_node_output(output);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "gl3009");
        assert!(!nodes[0].has_gpus);
        assert_eq!(nodes[1].name, "gl1000");
        assert!(nodes[1].has_gpus);
        assert_eq!(nodes[1].alloc_gpus, 2);
    }
}
