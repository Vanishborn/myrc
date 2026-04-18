# myrc Design Document

> Authoritative architectural reference for the `myrc` CLI toolkit.
> Current as of v0.3.1.
> Covers naming, project structure, module contracts, concurrency patterns,
> terminal aesthetics, dependencies, build, and deployment.
> Consult this document before writing or reviewing any code.

## Table of Contents

1. [Naming](#1-naming)
2. [Language: Rust](#2-language-rust)
3. [Repository Layout](#3-repository-layout)
4. [Common Module](#4-common-module-srccommoncommonrs)
5. [Terminal Aesthetics](#5-terminal-aesthetics)
6. [Module Specifications](#6-module-specifications)
7. [Concurrency Architecture](#7-concurrency-architecture)
8. [Dependencies](#8-dependencies-cargotoml)
9. [Performance Characteristics](#9-performance-characteristics)
10. [Backward Compatibility](#10-backward-compatibility)
11. [Build and Deployment](#11-build-and-deployment)
12. [Risks and Mitigations](#12-risks-and-mitigations)
13. [Exit Codes](#13-exit-codes)
14. [Diagnostics](#14-diagnostics)
15. [Testing](#15-testing)
16. [Coding Conventions](#16-coding-conventions)
17. [Target Slurm Version](#17-target-slurm-version)
18. [Future Work](#18-future-work)

---

## 1. Naming

The toolkit is named `myrc`, short for **"my resources."**

Every command operates on a cluster resource: compute allocations, job costs, usage metrics, storage analysis, or maintenance windows. The name preserves continuity with the `my_*` legacy command suite, reducing the learning curve for existing users.

---

## 2. Language: Rust

| Property       | Value                                                                                                |
| -------------- | ---------------------------------------------------------------------------------------------------- |
| Concurrent I/O | `tokio` async runtime; parallel subprocess fanout (8–12× on account-usage, 5–10× on account-running) |
| Deployment     | Single binary. No Python, Perl, virtualenvs, or `module load`                                        |
| Slurm data     | `sacct --json`, `scontrol`, `sacctmgr -P`, `sreport -P` via subprocess                               |
| CLI framework  | `clap` derive: subcommand routing, auto `--help`, shell completions                                  |
| Diagnostics    | `tracing` structured logging with `-v`/`-vv`/`-vvv` verbosity                                        |
| Memory safety  | No GC, no segfaults, no leaks; critical on shared HPC login nodes                                    |
| Startup        | ~1ms cold start vs. ~50–250ms for Python                                                             |

---

## 3. Repository Layout

```txt
myrc/
├── Cargo.toml
├── Makefile                            # build, test, release orchestration
├── build.rs                            # version metadata (shadow-rs)
├── bin/
│   └── main.rs                         # CLI entry point, clap root + dispatch
├── src/
│   ├── lib.rs                          # crate root, declares all modules
│   ├── common/
│   │   ├── mod.rs
│   │   └── common.rs                   # shared utilities (§4)
│   ├── accounts/
│   │   ├── mod.rs
│   │   └── accounts.rs                 # myrc accounts
│   ├── account_usage/
│   │   ├── mod.rs
│   │   └── account_usage.rs            # myrc account usage
│   ├── account_running/
│   │   ├── mod.rs
│   │   └── account_running.rs          # myrc account running
│   ├── usage/
│   │   ├── mod.rs
│   │   └── usage.rs                    # myrc usage
│   ├── job_estimate/
│   │   ├── mod.rs
│   │   └── job_estimate.rs             # myrc job estimate
│   ├── job_header/
│   │   ├── mod.rs
│   │   └── job_header.rs               # myrc job header
│   ├── job_stats/
│   │   ├── mod.rs
│   │   └── job_stats.rs                # myrc job stats
│   ├── job_list/
│   │   ├── mod.rs
│   │   └── job_list.rs                 # myrc job list
│   ├── modules_setup/
│   │   ├── mod.rs
│   │   └── modules_setup.rs            # myrc modules setup
│   ├── maxwalltime/
│   │   ├── mod.rs
│   │   └── maxwalltime.rs              # myrc maxwalltime
│   ├── sstate/
│   │   ├── mod.rs
│   │   └── sstate.rs                   # myrc sstate
├── docs/
│   ├── design.md                       # this document
│   └── adapting.md                     # porting guide for other institutions
└── dev/                                # development-only (git-ignored)
    ├── old/                            # predecessor scripts (reference)
    ├── others/                         # misc utilities (GUFI, etc.)
    ├── etc/                            # maintenance epoch data
    └── docs/                           # planning docs, pre-refactor notes
```

### 3.1 Module Convention

Every module directory follows:

```txt
src/{module_name}/
├── mod.rs              # pub mod {module_name}; pub use ...;
└── {module_name}.rs    # Args struct, run() function, private helpers
```

Each `{module_name}.rs` exports:

- `pub struct Args`: `clap` derive struct for the subcommand's flags/args
- `pub async fn run(args: &Args) -> Result<()>` (async modules) or `pub fn run(args: &Args) -> Result<()>` (sync modules)

### 3.2 Binary Entry Point: `bin/main.rs`

Responsibilities:

1. Define `clap` root command `myrc` with global options (`--json`, `-v`/`-vv`/`-vvv`)
2. Declare all subcommands via `#[derive(Subcommand)]`
3. Dispatch to module `run()` functions
4. Initialize tokio runtime only for async subcommands
5. Initialize `tracing` subscriber based on verbosity flags

```rust
#[derive(Parser)]
#[command(name = "myrc", about = "Unified CLI for UM HPC cluster resources")]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, global = true)]
    color: ColorMode,
    #[arg(short = 'v', long = "verbose", action = Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Accounts(accounts::Args),
    #[command(subcommand)]
    Account(AccountCommands),
    Usage(usage::Args),
    #[command(subcommand)]
    Job(JobCommands),
    #[command(subcommand)]
    Modules(ModulesCommands),
    Maxwalltime(maxwalltime::Args),
    Sstate(sstate::Args),
    Completions { shell: Shell },
    #[command(hide = true)]
    GenerateMan { dir: PathBuf },
    #[command(hide = true)]
    GenerateCompletions { dir: PathBuf },
}
```

Conditional tokio entry:

```rust
fn main() -> Result<()> {
    let cli = Cli::parse();
    // init tracing subscriber from -v count / $MYRC_LOG
    match cli.command {
        Commands::Account(sub) => {
            let rt = tokio::runtime::Runtime::new()?;
            match sub { /* dispatch to account_usage::run / account_running::run */ }
        }
        Commands::Accounts(args) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(accounts::run(&args, mode))
        }
        // ...
    }
}
```

---

## 4. Common Module (`src/common/common.rs`)

Shared infrastructure used across subcommands. These utilities enforce aesthetic and behavioral consistency: every module that formats dollars, parses memory, or renders a table delegates to the same function.

### 4.1 Slurm I/O

| Component              | Description                                                                   |
| ---------------------- | ----------------------------------------------------------------------------- |
| `slurm_cmd()`          | Async subprocess runner with 30s timeout. Returns `Result<String, MyrcError>` |
| `slurm_cmd_parallel()` | Fans out N `slurm_cmd` calls via `JoinSet`, capped at 12 concurrent           |
| `parse_slurm_kv()`     | Parses `sacctmgr -P`/`sreport -P` pipe-delimited output into `Vec<Vec<&str>>` |

**Timeout:** Each `slurm_cmd()` call has a **30-second** per-subprocess timeout (safety net for hung Slurm daemons). Override via `$MYRC_SLURM_TIMEOUT` (seconds).
On timeout, the child process is killed and `MyrcError::SlurmCmd` is returned with exit code 69 (service unavailable).

**Concurrency cap:** `slurm_cmd_parallel()` uses a `tokio::sync::Semaphore` with **12 permits** to avoid flooding the Slurm RPC endpoint on shared login nodes. This is separate from Slurm's server-side rate limiting; it's a client-side courtesy to avoid monopolizing RPC slots.

### 4.2 User & Account Resolution

| Component            | Description                                                                                             |
| -------------------- | ------------------------------------------------------------------------------------------------------- |
| `resolve_user()`     | Positional arg > `--user` flag > `$USER`. Single canonical pattern for all user-accepting modules       |
| `validate_account()` | `sacctmgr -p list Account` existence check. Returns early with a clear error message if account missing |

### 4.3 Date & Time

| Component          | Description                                                                                    |
| ------------------ | ---------------------------------------------------------------------------------------------- |
| `FiscalYear`       | Jul–Jun fiscal year math. Enumerates months, computes `sreport`/`sacct` start/end date strings |
| `DateRange`        | Validates and normalizes start/end dates for arbitrary month ranges                            |
| `parse_walltime()` | `"1-12:30:00"` → `Duration`. Handles DD-HH:MM:SS, HH:MM:SS, MM:SS                              |

### 4.4 Parsing & Conversion

| Component           | Description                                                                                                                                          |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `parse_memory()`    | `"10G"` → bytes as `u64`. Handles G/M/T/K suffixes (case-insensitive). `default_unit` sets bare-number semantics (1 for bytes, 1 MiB for Slurm TRES) |
| `billing_divisor()` | Returns 100,000 (pre-July 2021) or 10,000,000 (after) for a given date                                                                               |
| `compute_cost()`    | `billing × minutes / divisor` → `f64` dollars. Single formula, one place                                                                             |

### 4.5 Formatting

| Component           | Description                                                                        |
| ------------------- | ---------------------------------------------------------------------------------- |
| `format_dollars()`  | `f64` → `"$1,234.56"` (2 decimals, thousands separator). See §5.7                  |
| `format_percent()`  | `f64` → `"87.3%"` (1 decimal). See §5.7                                            |
| `format_memory()`   | Bytes → auto-scaled IEC unit: `"256 MiB"`, `"1.5 GiB"`, `"2.0 TiB"`. See §5.7      |
| `format_walltime()` | `Duration` → `DD-HH:MM:SS` (Slurm) or `HHH:MM:SS` (human-readable). See §5.7       |
| `Table`             | Aligned column output. Adapts to terminal width. Right-aligns numbers, left-aligns |
|                     | strings. Bold headers. Optional totals row with divider. Optional per-cell color   |
|                     | callback (`CellColorFn`) applied after padding to preserve alignment. See §5.7     |
| `TerminalInfo`      | Terminal width detection via `terminal_size`. `is_narrow()` → < 105 columns        |

### 4.6 Environment & Output Control

| Component          | Description                                                                                                                                                    |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ClusterEnv`       | Reads `$CLUSTER_NAME`, resolves epoch file paths via `$MYRC_ETC_DIR`. Cluster-specific feature flags                                                           |
| `OutputMode`       | JSON vs table output, driven by `--json` global flag. Gates formatting calls                                                                                   |
| `ColorMode`        | `Auto`/`Always`/`Never`, driven by `--color` global flag. `Auto`: colored crate decides (TTY + `NO_COLOR`). `Always`/`Never`: `set_override()` before dispatch |
| `SpinnerGroup`     | Preconfigured `MultiProgress` with braille charset and staggered tick intervals (§5.4)                                                                         |
| `confirm_prompt()` | `"message [y/N]: "` interactive prompt. Default-no. Returns `bool`. Skipped when `--yes` flag is set                                                           |
| `ExitCode`         | Standard exit code enum: 0 success, 1 failure, 2 usage, 69 service unavailable, 78 config, 130 interrupted                                                     |
| `MyrcError`        | Error enum via `thiserror`: SlurmCmd, Parse, Io, InvalidInput. Each variant maps to an `ExitCode` via `.exit_code()`. Propagate with `anyhow` + `.context()`   |

Parallel fanout pattern:

```rust
use tokio::sync::Semaphore;
use std::sync::Arc;

const MAX_CONCURRENT: usize = 12;
const DEFAULT_TIMEOUT_SECS: u64 = 30;

pub async fn slurm_cmd_parallel(cmds: Vec<Vec<String>>) -> Result<Vec<String>> {
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let mut set = JoinSet::new();
    for (i, cmd) in cmds.into_iter().enumerate() {
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            (i, slurm_cmd(&cmd).await)
        });
    }
    let mut results = vec![String::new(); set.len()];
    while let Some(res) = set.join_next().await {
        let (i, output) = res??;
        results[i] = output;
    }
    Ok(results)
}
```

---

## 5. Terminal Aesthetics

Clean, informational output. No ASCII banners, no large decorative elements.
Designed for HPC cluster users who expect Unix-tool-level simplicity.

**Color palette:** Derived from the [U-M Brand Color Guidelines](https://brand.umich.edu/design-resources/colors/) and tuned for dark terminal backgrounds. Three palette variants were evaluated (normal, grayish, bright); the **normal** set was chosen for its balance of scannability and comfort under repeated daily use.

| Semantic Role | Name          | Hex       | RGB             | ANSI-16 Fallback |
| ------------- | ------------- | --------- | --------------- | ---------------- |
| Error         | Tomato Jam    | `#c8352a` | (200, 53, 42)   | Red              |
| Warning       | Chocolate     | `#e27328` | (226, 115, 40)  | Yellow           |
| Accent/Maize  | Bright Amber  | `#f5c518` | (245, 197, 24)  | Yellow           |
| Success       | Medium Jungle | `#5ba84f` | (91, 168, 79)   | Green            |
| Info/Spinner  | Blue Bell     | `#4f95c9` | (79, 149, 201)  | Cyan             |
| Purple accent | Deep Lilac    | `#7c4499` | (124, 68, 153)  | Magenta          |
| Dim/Elapsed   | Rosy Granite  | `#8e9094` | (142, 144, 148) | Dim              |

**Truecolor detection:** A `LazyLock<bool>` static checks `$COLORTERM` once for `truecolor` or `24bit`. When available, use `.truecolor(r, g, b)` from the `colored` crate. Otherwise fall back to ANSI-16 equivalents. `NO_COLOR` suppresses all color regardless.

### 5.1 Crates

| Crate       | Version | Purpose                                           |
| ----------- | ------- | ------------------------------------------------- |
| `colored`   | 2       | Colorize strings: `.green()`, `.red()`, `.bold()` |
| `indicatif` | 0.17    | Progress spinners with live-updating counters     |

### 5.2 Color Semantics

| Context      | Palette Color           | ANSI Fallback | Example                                  |
| ------------ | ----------------------- | ------------- | ---------------------------------------- |
| Phase header | **bold white**          | bold white    | `"Querying 12 months...".bold()`         |
| Success      | Medium Jungle `#5ba84f` | green         | `"Done.".green()`                        |
| Error        | Tomato Jam `#c8352a`    | red           | `"Error: invalid account".red()`         |
| Warning      | Chocolate `#e27328`     | yellow        | `"Maintenance window imminent".yellow()` |
| Info/label   | Blue Bell `#4f95c9`     | cyan          | Spinner prefix for totals/counts         |
| Elapsed time | Rosy Granite `#8e9094`  | dim           | `{elapsed:.dim}` in spinner template     |
| Accent       | Bright Amber `#f5c518`  | yellow        | Highlight, emphasis (reserved)           |
| Accent 2     | Deep Lilac `#7c4499`    | magenta       | Reserved for future use                  |

Colors are suppressed when stdout is not a TTY (piped output) via the `colored` crate's auto-detection. Honor the `NO_COLOR` environment variable ([no-color.org](https://no-color.org/)). Use `std::io::IsTerminal` for TTY detection; `terminal_size` only for width measurement.

The `--color=auto|always|never` global flag overrides auto-detection:

- `auto` (default): `colored` crate decides with ON for TTY, OFF for pipe/redirect, OFF if `NO_COLOR=1`.
- `always`: force color ON even through pipes (`myrc sstate --color=always | less -R`).
- `never`: force color OFF on a TTY, per-invocation alternative to `NO_COLOR=1`.

Implemented via `colored::control::set_override(bool)` before command dispatch.

### 5.3 Semantic Data Coloring

Color is a signal, not decoration. Applied only to human output; JSON is never colored.

| Context                | Rule                                                                   | Function             |
| ---------------------- | ---------------------------------------------------------------------- | -------------------- |
| Job state (any module) | COMPLETED→green, RUNNING→blue, PENDING/TIMEOUT→orange, FAILED/etc.→red | `color_job_state()`  |
| Efficiency (job stats) | ≥75%→green, ≥25%→orange, <25%→red                                      | `color_efficiency()` |
| Exit code (job stats)  | `0` or `0:0`→green, anything else→red                                  | `color_exit_code()`  |
| Node state (sstate)    | IDLE→green, DOWN/NOT_RESPONDING→red, DRAIN/MAINTENANCE/etc.→orange     | inline callback      |
| Bottleneck (sstate)    | All avail=0 on available node→red avail cells                          | inline callback      |
| Positive result        | "No maintenance window" → green                                        | `color_success()`    |
| Tips (job stats)       | Dim text for resource optimization suggestions                         | `color_dim()`        |

Table coloring uses `Table::set_cell_color()`, a callback invoked with `(row_idx, col_idx, padded_str)` **after** padding. This preserves column alignment: ANSI escapes are injected around already-padded text, so `cell.len()` width calculations remain correct.

### 5.4 Spinners and Progress

Used for subcommands with concurrent I/O where the user waits >1 second.

**Spinner character set:** Braille dots: `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`

**Template:**

```rs
{spinner:.COLOR} {prefix:.bold:<12} {msg:.bold.COLOR:>6} {elapsed:.dim}
```

**Color-coded multi-spinners** (via `indicatif::MultiProgress`):

| Metric  | Color     | Example use                      |
| ------- | --------- | -------------------------------- |
| Total   | **cyan**  | Total months queried, total jobs |
| Success | **green** | Completed queries, matched jobs  |
| Failed  | **red**   | Timed-out queries, parse errors  |

**Staggered tick intervals:** Each spinner in a `MultiProgress` group ticks at a slightly different rate (100ms, 120ms, 140ms, ...) to prevent visual lockstep pulsing. Offset = `100 + (index × 20)` ms.

**When to use spinners:**

| Subcommand        | Spinner? | Reason                                      |
| ----------------- | -------- | ------------------------------------------- |
| `account usage`   | Yes      | 12 concurrent month queries, 2–5s wait      |
| `account running` | Yes      | N concurrent per-job queries, variable wait |
| `usage`           | Yes      | Single `sreport` call, ~2s wait             |
| `job stats`       | Yes      | `sacct --json` call, ~2s wait               |
| All others        | No       | Single query, <1s; spinner adds clutter     |

Spinners are cleared on completion; the final line shows count + unit:

```txt
⠹ Total:    12 months  3.2s
⠸ Success:  12 months  3.2s
```

### 5.5 Output Phases

Multi-step subcommands (e.g., `account usage`) separate phases with a leading `\n`:

```txt
Validating account...
Done.

Querying 12 months...
⠋ Total:     0 months  0.1s
⠙ Success:   0 months  0.1s
...
Done.

Generating report...
```

Single-step subcommands print results directly with no phase headers.

### 5.6 `--json` Global Flag

When `--json` is passed, all human-readable formatting (colors, spinners, phase headers) is suppressed. Output is a single JSON object to stdout. Spinners and status messages go to stderr (if any).

Per-module JSON schemas are defined alongside each module's implementation.

### 5.7 Formatting Conventions

Every number, unit, and table in the toolkit uses the same formatting rules.
This is what makes `myrc` feel like one product.

| Element          | Format                          | Example                         |
| ---------------- | ------------------------------- | ------------------------------- |
| Dollar amounts   | `$X,XXX.XX` (2 decimals)        | `$1,234.56`, `$0.03`            |
| Percentages      | `XX.X%` (1 decimal)             | `87.3%`, `0.0%`, `100.0%`       |
| Memory (human)   | Auto-scaled IEC unit, 1 decimal | `256 MiB`, `1.5 GiB`, `2.0 TiB` |
| Memory (Slurm)   | Slurm-native suffix             | `768M`, `4G`, `1T`              |
| Walltime (Slurm) | `DD-HH:MM:SS`                   | `01-12:30:00`                   |
| Walltime (human) | `HHH:MM:SS`                     | `036:30:00`                     |
| Dates            | `YYYY-MM-DD`                    | `2026-01-15`                    |
| Job IDs          | Numeric, array: `JOBID_TASKID`  | `12345678`, `12345678_42`       |
| Counts           | Plain integer (no separator)    | `128`, `1024`                   |

**Table conventions:**

- Column headers: **bold** (via `colored`). No underline, no box drawing.
- Numbers: right-aligned. Strings: left-aligned.
- Column separation: 2 spaces minimum.
- Totals row: preceded by a dash divider (`──────...`), totals in **bold**.
- Adaptive width: when terminal < 105 columns, switch to key-value blocks (§1 `accounts` pattern). Modules that always produce narrow output skip this.
- Empty results: dim message, exit 0. Example: `"No running jobs for account arc-ts.".dimmed()`

**Report conventions (non-table key-value output):**

- Shared divider: `DIVIDER` constant in `common.rs` with 72 Unicode `─` characters. Used by all report-style and table-header output.
- Section titles: **bold** (via `colored`), followed by `DIVIDER` on next line.
- Label alignment: `{:<20}` format specifier (20-char left-padded label column).
- Continuation lines: 20-char indent (`{:<20}` with empty string) for multi-line values.
- Closing divider: `DIVIDER` at end of report (after any trailing notes).
- Walltime breakdown (job estimate): left-padded to 2 chars (`{:<2}`) so unit labels align vertically while the leading digit aligns with all other key-value pairs.
- Cost values: **bold** in standalone key-value lines (via `.bold()` on `format_dollars()` result). Not colored. Bold inherits terminal foreground, safe on dark and light backgrounds.
- Section separation: blank lines between logical groups (e.g., path fields → time fields, state → resources → efficiency).
- Title line identifiers: key values (job_id, cluster, user, account) colored with `color_info().bold()` (blue + bold) to distinguish from surrounding bold text.
- Exit code: `"0"` or `"0:0"` → green (`color_success`), all others → red (`color_error`).

**Error display conventions:**

- Error prefix: `error:` in **red bold**, followed by message in default color. Matches `clap` and `rustc` conventions.
- Warning prefix: `warning:` in **yellow bold**.
- All errors and warnings go to stderr.
- Context chaining via `anyhow`: `"querying billing for {account}"` appears as indented cause chain when `-vv` is active.

---

## 6. Module Specifications

### 6.1 `accounts`

|             |                                                  |
| ----------- | ------------------------------------------------ |
| **Command** | `myrc accounts [USER]`                           |
| **Runtime** | tokio                                            |
| **Calls**   | `sacctmgr list assoc` (single, async)            |
| **Notes**   | Adaptive output for narrow terminals (<105 cols) |

### 6.2 `account usage`

|             |                                                                                                                                     |
| ----------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| **Command** | `myrc account usage -a ACCOUNT [-y YEAR] [-s START -e END] [-t TYPE] [-p] [--sort-by-total\|--sort-by-current\|--sort-by-previous]` |
| **Runtime** | tokio                                                                                                                               |
| **Calls**   | `sacctmgr` (validation + limit) + `sreport` × N months (concurrent)                                                                 |
| **Notes**   | Fiscal year July–June. Billing divisor changes July 2021. 8–12× speedup via parallel month fanout                                   |

### 6.3 `account running`

|             |                                                                                                                      |
| ----------- | -------------------------------------------------------------------------------------------------------------------- |
| **Command** | `myrc account running -a ACCOUNT [-d]`                                                                               |
| **Runtime** | tokio                                                                                                                |
| **Calls**   | `squeue` (job list) + `scontrol show job` × N (concurrent)                                                           |
| **Notes**   | Memory unit normalization (M→G, T→G). `-d`/`--detail`: per-job breakdown. 5–10× speedup via parallel per-job queries |

### 6.4 `usage`

|             |                                                |
| ----------- | ---------------------------------------------- |
| **Command** | `myrc usage [USER] [--year YYYY] [--month MM]` |
| **Runtime** | tokio                                          |
| **Calls**   | `sreport` (single)                             |
| **Notes**   | Lighthouse cluster exclusion                   |

### 6.5 `job estimate`

|             |                                                                                                  |
| ----------- | ------------------------------------------------------------------------------------------------ |
| **Command** | `myrc job estimate -p PARTITION [-c CORES] [-g GPUS] [-m MEM] [-t TIME] [-n NODES]`              |
| **Runtime** | tokio                                                                                            |
| **Calls**   | `scontrol show partition` (single)                                                               |
| **Notes**   | `billing = max(cpu_w × cpus, mem_w × mem_gb, gpu_w × gpus)`. GPU mode inferred from `--gpus > 0` |

### 6.6 `job header`

|             |                                                                                                     |
| ----------- | --------------------------------------------------------------------------------------------------- |
| **Command** | `myrc job header`                                                                                   |
| **Runtime** | sync                                                                                                |
| **Calls**   | Reads `$SLURM_*` env vars; `nvidia-smi -L`, `ulimit -a`, `module list` via subprocess               |
| **Notes**   | Output is byte-identical to predecessor for log parseability. Called from job scripts (not sourced) |

### 6.7 `job list`

|             |                                                                                                            |
| ----------- | ---------------------------------------------------------------------------------------------------------- |
| **Command** | `myrc job list [-u USER] [-y YEAR] [-m MONTH] [-d DAY] [-t TYPE] [-a ACCOUNT] [--sort-by KEY] [--limit N]` |
| **Runtime** | tokio                                                                                                      |
| **Calls**   | `sacct` with date/state/account filters                                                                    |
| **Notes**   | Lists user's jobs with filtering + sorting. Complements `job stats` auto-detect                            |

### 6.8 `job stats`

|             |                                                                                                      |
| ----------- | ---------------------------------------------------------------------------------------------------- |
| **Command** | `myrc job stats [JOBID] [--raw]`                                                                     |
| **Runtime** | tokio                                                                                                |
| **Calls**   | `sacct` (auto-detect last job) + `sacct --json` (full record)                                        |
| **Notes**   | Defaults to user's most recent job if JOBID omitted. `--raw` dumps JSON. CPU/mem efficiency + cost.  |
|             | State and exit code are semantically colored (§5.3). Efficiency percentages color-coded by threshold |

Human-readable report layout:

```txt
Submit command:      sbatch scripts/run.sbatch
Working directory:   /home/user/projects/ml-experiment
Script path:         /home/user/projects/ml-experiment/scripts/run.sbatch
Job submit time:     04/13/2026 15:45:56
Queue wait time:     00:00:01
Job start time:      04/13/2026 15:45:57
Job end time:        04/13/2026 15:46:09
Job running time:    00:00:12
Walltime requested:  1-00:00:00

State:               COMPLETED
Exit code:           0
Stdout:              /home/user/logs/qc_12345.out
Stderr:              /home/user/logs/qc_12345.err

Account:             example_class
Partition:           standard
On nodes:            node001
                     (1 nodes with 2 cores per node)

CPU Utilized:        00:00:12
                     00:00:09 user, 00:00:02 system
CPU Efficiency:      50.00% of 00:00:24 total CPU time (cores * walltime)

Memory Utilized:     265.64 MiB
Memory Efficiency:   44.27% of 600.00 MiB

Max Disk Read:       1.12 GiB
Max Disk Write:      0.54 GiB
Cost:                $0.00

TIP: Job used 265.64 MiB of 600.00 MiB memory. Consider requesting less.
```

Fields sourced from `sacct --json` (Slurm 25+):

| Field             | JSON path                                         |
| ----------------- | ------------------------------------------------- |
| Submit command    | `submit_line`                                     |
| Queue wait        | `time.start` - `time.submission`                  |
| User/system CPU   | `time.user`, `time.system`                        |
| Walltime request  | `time.limit`                                      |
| Max disk read     | `steps[].tres.requested.max` (type=fs, name=disk) |
| Max disk write    | `steps[].tres.consumed.max` (type=fs, name=disk)  |
| Stdout/stderr     | `stdout_expanded`, `stderr_expanded`              |
| Account/partition | `account`, `partition`                            |

### 6.9 `modules setup`

|             |                                                                                                                           |
| ----------- | ------------------------------------------------------------------------------------------------------------------------- |
| **Command** | `myrc modules setup [-y]`                                                                                                 |
| **Runtime** | sync                                                                                                                      |
| **Notes**   | Checks `use.own` availability. `y/N` prompt (N default). `--yes` skips prompt. Creates `~/Lmod/hello/1.0.lua`. Idempotent |

### 6.10 `maxwalltime`

|             |                                                                                                          |
| ----------- | -------------------------------------------------------------------------------------------------------- |
| **Command** | `myrc maxwalltime [-S]`                                                                                  |
| **Runtime** | sync                                                                                                     |
| **Notes**   | `-S`/`--slurm-format` for machine-readable output. 6-hour buffer, 14-day cap. `chrono-tz` for US/Eastern |

### 6.11 `sstate`

|             |                                                                                                            |
| ----------- | ---------------------------------------------------------------------------------------------------------- |
| **Command** | `myrc sstate [-p PARTITION] [--raw]`                                                                       |
| **Runtime** | tokio                                                                                                      |
| **Calls**   | `scontrol show node -o` or `sinfo -N ...`                                                                  |
| **Notes**   | Per-node resource dashboard: CPU/mem/GPU alloc + avail + percent + load + state. Totals row.               |
|             | `--raw` disables bottleneck rule and state filtering. Node state and bottleneck avail cells colored (§5.3) |

---

## 7. Concurrency Architecture

Tokio is used selectively. Subcommands that benefit from async I/O enter the runtime. Sync subcommands avoid the ~0.5ms tokio startup.

| Module            | Runtime | Reason                               |
| ----------------- | ------- | ------------------------------------ |
| `accounts`        | tokio   | `sacctmgr list assoc` async call     |
| `account_usage`   | tokio   | 12 concurrent `sreport` calls        |
| `account_running` | tokio   | N concurrent `scontrol` calls        |
| `job_estimate`    | tokio   | `scontrol show partition` async call |
| `job_list`        | tokio   | `sacct` async call with filters      |
| `job_stats`       | tokio   | `sacct --json` async call            |
| `sstate`          | tokio   | `scontrol show node` async call      |
| `usage`           | tokio   | `sreport` async call                 |

### 7.1 Signal Handling & Graceful Shutdown

Currently, `SIGPIPE` is reset to `SIG_DFL` in `main()` so piping into `head`/`tail` exits cleanly. Ctrl+C terminates the process group, which also kills spawned child processes. All async subprocesses use `.kill_on_drop(true)` to ensure child processes are cleaned up when a timeout fires or the future is cancelled.

---

## 8. Dependencies (`Cargo.toml`)

```toml
[package]
name = "myrc"
version = "0.3.1"
edition = "2024"
rust-version = "1.85"
publish = false
license = "GPL-3.0-or-later"
description = "Unified CLI for UM HPC cluster resources"
categories = ["command-line-utilities"]
exclude = ["/dev/", "/docs/", "/dist/"]

[[bin]]
name = "myrc"
path = "bin/main.rs"

[dependencies]
clap = { version = "4", features = ["derive", "wrap_help", "color"] }
clap_complete = "4"
clap_mangen = "0.2"
tokio = { version = "1", features = ["rt-multi-thread", "process", "sync", "time"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
chrono = "0.4"
chrono-tz = "0.10"
anyhow = "1"
thiserror = "2"
terminal_size = "0.4"
colored = "2"
indicatif = "0.17"
human-panic = "2"
libc = "0.2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
shadow-rs = { version = "1", default-features = false }

[build-dependencies]
shadow-rs = { version = "1", features = ["build"] }

[target.'cfg(all(target_os = "linux", target_env = "gnu"))'.dependencies]
tikv-jemallocator = { version = "0.6", optional = true }

[features]
default = ["jemalloc"]
jemalloc = ["tikv-jemallocator"]

[profile.release]
opt-level = 3
lto = true
strip = true
codegen-units = 1
panic = "abort"

[lints.clippy]
enum_glob_use = "deny"
```

The single `unsafe` block in `bin/main.rs` resets `SIGPIPE` to `SIG_DFL`. Required because Rust ignores `SIGPIPE` by default, causing panics when piping into `head`/`tail`.

| Crate                | Purpose                                           |
| -------------------- | ------------------------------------------------- |
| `clap`               | Unified CLI, subcommands, shell completions       |
| `clap_complete`      | Static shell completion scripts (bash/zsh/fish)   |
| `clap_mangen`        | Man page generation from clap CLI definitions     |
| `tokio`              | Async concurrent subprocess fanout                |
| `serde`/`serde_json` | `sacct --json` parsing, `--raw` dump              |
| `chrono`/`chrono-tz` | Timezone-aware date math                          |
| `anyhow`             | Ergonomic error propagation with `.context()`     |
| `thiserror`          | Derive `std::error::Error` for `MyrcError`        |
| `terminal_size`      | Terminal width measurement                        |
| `colored`            | Colorized status/error/phase output               |
| `indicatif`          | Braille spinners with live counters               |
| `human-panic`        | User-friendly crash reports to temp file          |
| `libc`               | SIGPIPE reset on Unix                             |
| `tracing`            | Structured diagnostic logging (`-v`/`-vv`/`-vvv`) |
| `tracing-subscriber` | Log output formatting + `$MYRC_LOG` env filter    |
| `shadow-rs`          | Git hash + build timestamp in `--version`         |
| `tikv-jemallocator`  | Better allocator on Linux HPC nodes               |

---

## 9. Performance Characteristics

| Subcommand        | Typical Latency | Technique                        |
| ----------------- | --------------- | -------------------------------- |
| `account usage`   | ~2–5s           | 12 concurrent `sreport` calls    |
| `account running` | ~0.2–1s         | N concurrent `scontrol show job` |
| `usage`           | ~1–2s           | Single `sreport` call            |
| `job stats`       | ~1–2s           | `sacct --json` single call       |
| `maxwalltime`     | ~2ms            | Pure computation, no Slurm calls |
| All others        | ~1–50ms + Slurm | Single subprocess call           |

---

## 10. Backward Compatibility

### 10.1 Shell Aliases

```bash
alias my_accounts='myrc accounts'
alias my_account_billing='myrc account usage'        # absorbed into account usage
alias my_account_billing_data='myrc account usage'
alias my_account_usage='myrc account usage'
alias my_account_resources='myrc account running'
alias my_job_estimate='myrc job estimate'
alias my_job_header='myrc job header'
alias my_job_statistics='myrc job stats'
alias my_modules_setup='myrc modules setup'
alias my_usage='myrc usage'
alias maxwalltime='myrc maxwalltime'
alias sstate='myrc sstate'
```

### 10.2 Data Files

`etc/` epoch files read from `$MYRC_ETC_DIR`. The compiled-in default points to the legacy slurm-usertools path (`/sw/pkgs/arc/usertools/etc/`); IT may override this at deployment time.

---

## 11. Build and Deployment

### 11.1 `build.rs`

Generates **version metadata** via `shadow-rs`, embedding git hash and build timestamp into `--version` output.

### 11.2 Build Commands

```bash
# Standard build (no Slurm headers needed)
cargo build --release

# Cross-compile for cluster (via cargo-zigbuild)
cargo zigbuild --release --target x86_64-unknown-linux-gnu

# Without jemalloc (e.g., macOS dev)
cargo build --release --no-default-features

# Deploy
scp target/x86_64-unknown-linux-gnu/release/myrc <cluster>:/sw/bin/myrc
```

### 11.3 Distribution

#### Versioning

Versions follow **Semantic Versioning 2.0.0** (`MAJOR.MINOR.PATCH`):

- `MAJOR`: breaking changes to CLI interface, JSON output schemas, or exit codes
- `MINOR`: new subcommands, new flags, new JSON fields (backward-compatible)
- `PATCH`: bug fixes, performance improvements, output formatting tweaks

The canonical version lives in `Cargo.toml` (`version = "X.Y.Z"`). Git tags (`v0.1.0`, `v0.2.0`, `v0.3.0`, `v0.3.1`, etc.) mark releases. `shadow-rs` embeds the build target into `--version` so deployed binaries are always traceable:

```zsh
myrc 0.3.1
Author: Haoran "Henry" Li @ University of Michigan
Target: x86_64-unknown-linux-gnu
```

#### Target Platforms

All Slurm-supporting Linux platforms:

| Rust Target Triple              | Platform      | Notes                                |
| ------------------------------- | ------------- | ------------------------------------ |
| `x86_64-unknown-linux-gnu`      | Linux x86_64  | Primary. All current UM clusters     |
| `aarch64-unknown-linux-gnu`     | Linux ARM64   | AWS Graviton, emerging HPC ARM nodes |
| `powerpc64le-unknown-linux-gnu` | Linux ppc64le | IBM POWER9/10 (Summit-class systems) |

The `jemalloc` feature is auto-enabled on all Linux targets via the existing `cfg(all(target_os = "linux", target_env = "gnu"))` gate in `Cargo.toml`.

#### Cross-Compilation Toolchain

**`cargo-zigbuild`** ([rust-cross/cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)) is used for building Linux binaries from macOS:

- Uses **Zig** as a drop-in cross-linker for C code (jemalloc, etc.)
- Two lightweight tools on the host, no containers or VMs needed
- Can pin a minimum glibc version (e.g., `--target x86_64-unknown-linux-gnu.2.17` for RHEL 7 / CentOS 7 compatibility) if needed
- Supports all three target triples above
- Install:

  ```bash
  brew install zig
  cargo install cargo-zigbuild
  ```

- Rustup targets are added automatically by the Makefile (`rustup target add`)

#### Release Artifacts

The `Makefile` at the project root orchestrates multi-platform release builds:

```bash
make release          # Build all platforms, package tarballs, generate checksums
make build            # Dev build (native, debug)
make build-release    # Native release build
make clean            # Remove build artifacts
make test             # Run tests + clippy
make install          # Install to $CARGO_HOME/bin
make install PREFIX=<path>
                      # Full install: binary + man pages + shell completions
```

Each platform produces:

```txt
dist/
├── myrc_<VERSION>_x86_64-unknown-linux-gnu.tar.gz
├── myrc_<VERSION>_aarch64-unknown-linux-gnu.tar.gz
├── myrc_<VERSION>_powerpc64le-unknown-linux-gnu.tar.gz
└── checksums.txt
```

`<VERSION>` is the output of `git describe --tags --always --dirty`, e.g., `v0.1.0` (tagged release) or `v0.1.0-3-gabc1234` (post-tag commit).

Each tarball contains:

- `myrc`: the statically-optimized release binary
- `LICENSE`: license file

`checksums.txt` contains SHA-256 hashes of all tarballs for integrity verification on the cluster after `scp`:

```bash
sha256sum -c checksums.txt
```

#### Deployment to Cluster

```bash
# Build release for the primary platform
make release

# Deploy to cluster (paths determined by IT)
scp dist/myrc_<VERSION>_x86_64-unknown-linux-gnu.tar.gz <login-node>:/tmp/
ssh <login-node>
tar xzf /tmp/myrc_<VERSION>_x86_64-unknown-linux-gnu.tar.gz -C <install-dir>/
```

#### GitHub Releases

Tag and push to create a release:

```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
# Upload dist/ artifacts to GitHub Release page
```

---

## 12. Risks and Mitigations

| Risk                     | Mitigation                                                        |
| ------------------------ | ----------------------------------------------------------------- |
| Slurm CLI output changes | Tested against expected output. `--json` flag for machine parsing |
| Cross-compilation        | `cargo-zigbuild` + Zig for Linux targets from macOS, lightweight  |
| Color on non-TTY         | `colored` crate auto-detects TTY; honors `NO_COLOR`               |

---

## 13. Exit Codes

| Code | Meaning             | When                                            |
| ---- | ------------------- | ----------------------------------------------- |
| 0    | Success             |                                                 |
| 1    | General failure     | Slurm query error, parse failure, invalid input |
| 2    | Usage error         | Bad arguments (clap handles automatically)      |
| 69   | Service unavailable | Slurm daemon down, RPC timeout                  |
| 78   | Config error        | Missing epoch file, bad `$CLUSTER_NAME`         |
| 130  | Interrupted         | Ctrl+C (`128 + SIGINT`)                         |

Return `Result` from all module `run()` functions. `MyrcError` carries an `exit_code()` method mapping each variant to the appropriate code (`InvalidInput` → 1, `SlurmCmd` → per-instance, `Parse`/`Io` → 1). `main()` downcasts `anyhow::Error` to `MyrcError` to extract the code; unknown errors default to 1. Exit code 2 is reserved for clap's own CLI parsing errors.

---

## 14. Diagnostics

- **`human-panic`** is initialized in `main()`. Panics write a crash report to a temp file instead of dumping a raw backtrace to the terminal.
- **Error context** is provided via `anyhow`'s `.context()` method, producing cause chains like `"querying billing for {account}"`.
- **`$MYRC_SLURM_TIMEOUT`** overrides the default 30-second per-subprocess timeout for debugging slow Slurm daemons.
- **Structured logging** via `tracing` + `tracing-subscriber`, controlled by the global `-v` flag:
  - Default (no flag): warnings only
  - `-v`: info; shows user/account resolution, parallel fanout counts
  - `-vv`: debug; shows every Slurm command spawned with args and timeout
  - `-vvv`: trace; shows command output sizes and full resolution detail
  - `$MYRC_LOG` overrides the flag-based level (e.g., `MYRC_LOG=trace`). All log output goes to stderr.

---

## 15. Testing

### 15.1 Unit Tests

`#[cfg(test)]` modules alongside each source file. Test parsing, formatting, data transforms, walltime/memory conversion, billing divisor logic. Fixture-grounded tests use representative `scontrol`, `sacct`, and `sreport` output captured from production clusters to validate parsers against real-world data.

### 15.2 CI Pipeline

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test --all
```

---

## 16. Coding Conventions

- Derive `Debug`, `Clone`, `Default` on all public structs.
- All error types must be `Send + Sync + 'static` (required by `anyhow`, tokio).
- `thiserror` for defining error enums; `anyhow` for propagating with `.context("querying billing for {account}")`.
- Use `human-panic` in `main()`. Panics write a crash report to a temp file instead of dumping a raw backtrace.
- Prefer `std::io::IsTerminal` for TTY detection. Use `terminal_size` only for width.
- `shadow-rs` embeds git hash and build date in `--version`. Useful for identifying deployed builds on cluster nodes.

---

## 17. Target Slurm Version

UMich clusters run **Slurm 25.11.1**. This is well past the 21.08 threshold for `sacct --json` support. Implications:

- `sacct --json` is the **primary** data path for `job stats`. No need for a `sacct -P` fallback, simplifying parsing to `serde_json` only.
- `sreport --json` is **not** available. `sreport` still only supports pipe-delimited output. `account usage` continues to parse `-P` output.

---

## 18. Future Work

Items deferred from v0.1.0. Revisit after all current modules are stable.

| ID  | Area                | Description                                                                                                                                                                                                                                                                                         |
| --- | ------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| T-1 | `modules setup`     | Template selection menu: offer hello/python-venv/conda/custom starters                                                                                                                                                                                                                              |
| T-2 | `grufi`             | Full Rust rewrite of GUFI storage analysis tooling                                                                                                                                                                                                                                                  |
| T-3 | Timeout tuning      | Tune 30s timeout and 12-concurrent cap based on production experience (§4.1, §7)                                                                                                                                                                                                                    |
| T-4 | JSON schema freeze  | Version-stamp and freeze JSON schemas for machine-consumer stability. Post-v0.1.0 after real usage stabilizes schemas                                                                                                                                                                               |
| T-5 | Dynamic completions | Add custom `clap_complete` completers for flags that accept Slurm-queryable values: `-a` (accounts via `sacctmgr`), `-p` (partitions via `scontrol`). Each tab press runs a live Slurm query (~100–500ms). Only functional on cluster nodes                                                         |
| T-6 | Array job handling  | `job stats`: detect when `sacct --json -j BASE_ID` returns multiple array tasks and warn instead of silently taking the first. Show `array_job_id`/`array_task_id` in `print_report` title when set. `job header`: add `SLURM_ARRAY_JOB_ID` and `SLURM_ARRAY_TASK_ID` to the displayed env var list |
