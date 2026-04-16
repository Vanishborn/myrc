# Adapting myrc for Other Institutions

This document catalogs everything in the codebase that is specific to the University of Michigan HPC environment. If you are forking myrc for another institution, work through each section below.

---

## 1. Cluster Names

myrc recognizes cluster names via the `$CLUSTER_NAME` environment variable (set automatically on UMich login nodes). Several modules contain cluster-specific logic:

| Cluster      | Special behavior                                                                                                     |
| ------------ | -------------------------------------------------------------------------------------------------------------------- |
| `greatlakes` | Primary cluster. No special-case logic.                                                                              |
| `armis2`     | No special-case logic.                                                                                               |
| `lighthouse` | No billing TRES. `usage` is disabled, `account_usage` swaps billing to cpu TRES, `job_stats` skips cost calculation. |

**Files to edit:**

- `src/common/common.rs`: `ClusterEnv` struct, `is_lighthouse()` method
- `src/usage/usage.rs`: Lighthouse exit guard
- `src/account_usage/account_usage.rs`: Lighthouse TRES swap
- `src/job_stats/job_stats.rs`: cost calculation Lighthouse exclusion

If your site has no cluster-level TRES exceptions, remove the Lighthouse branches entirely.

---

## 2. Billing Formula

UMich converts raw Slurm TRES-minutes into dollar costs using a divisor that changed on July 1, 2021:

| Period          | Divisor    |
| --------------- | ---------- |
| Before Jul 2021 | 100,000    |
| Jul 2021 onward | 10,000,000 |

The cutoff and divisors are constants in `src/common/common.rs`:

```rust
const BILLING_DIVISOR_OLD: u64 = 100_000;
const BILLING_DIVISOR_NEW: u64 = 10_000_000;
const BILLING_CUTOFF: (i32, u32, u32) = (2021, 7, 1);
```

`job_estimate` and `job_stats` use only the current divisor (10M). If your site has a different rate structure, update these constants and the `billing_divisor()` function.

The environment variable `$MY_ACCOUNT_DIVISOR` overrides the divisor on a per-invocation basis (used in `account_usage`).

---

## 3. Fiscal Year

UMich operates on a July-to-June fiscal year. The `FiscalYear` struct in `src/common/common.rs` defines:

- Start: July 1
- End: June 30
- Month sequence: Jul, Aug, Sep, Oct, Nov, Dec, Jan, Feb, Mar, Apr, May, Jun

If your institution uses a calendar year or a different fiscal boundary, update `FiscalYear::start_date()`, `end_date()`, and `months()`.

---

## 4. Filesystem Paths

Two hardcoded paths assume the UMich `/sw/` tree:

| Constant             | Value                             | Used by         |
| -------------------- | --------------------------------- | --------------- |
| `DEFAULT_ETC_DIR`    | `/sw/pkgs/arc/usertools/etc/`     | `maxwalltime`   |
| `EXAMPLE_MODULE_SRC` | `/sw/examples/Lmod/hello/1.0.lua` | `modules setup` |

`DEFAULT_ETC_DIR` can be overridden at runtime via `$MYRC_ETC_DIR`. `EXAMPLE_MODULE_SRC` is compiled in; edit the constant in `src/modules_setup/modules_setup.rs`.

---

## 5. Maintenance Epoch Files

`maxwalltime` reads a Unix timestamp from a file named `{cluster}_next_maintenance_epochtime` inside the etc directory. The file contains a single integer (seconds since epoch) representing the next scheduled downtime.

If your site does not use this convention, replace the file-based approach in `src/maxwalltime/maxwalltime.rs` with whatever mechanism advertises your maintenance windows.

Related constants:

| Constant       | Value | Meaning                                   |
| -------------- | ----- | ----------------------------------------- |
| `MAX_DAYS`     | 14    | Cap walltime at 14 days if no maintenance |
| `BUFFER_HOURS` | 6     | Subtract 6 hours before maintenance start |

---

## 6. Partition Defaults

`job_estimate` defaults to partition `standard` and memory `768mb` (the per-core default on Great Lakes). Edit the `#[arg]` defaults in `src/job_estimate/job_estimate.rs` to match your site.

GPU validation checks whether the partition name contains `"gpu"`. If your GPU partitions follow a different naming convention, update the substring check in the same file.

---

## 7. Environment Variables

| Variable              | Required | Default                       | Purpose                                          |
| --------------------- | -------- | ----------------------------- | ------------------------------------------------ |
| `$CLUSTER_NAME`       | Yes      | (none)                        | Identifies the current cluster                   |
| `$MYRC_ETC_DIR`       | No       | `/sw/pkgs/arc/usertools/etc/` | Maintenance epoch file directory                 |
| `$MYRC_SLURM_TIMEOUT` | No       | 30 (seconds)                  | Per-subprocess timeout                           |
| `$MY_ACCOUNT_DIVISOR` | No       | (computed from date)          | Override billing divisor                         |
| `$USER`               | No       | (system)                      | Fallback for current username                    |
| `$HOME`               | No       | (system)                      | Lmod module directory base                       |
| `$MYRC_LOG`           | No       | (none)                        | Tracing filter override (`debug`, `trace`, etc.) |
| `$COLORTERM`          | No       | (none)                        | Truecolor detection (`truecolor` or `24bit`)     |

---

## 8. Slurm Version

myrc targets Slurm 25.11+ and uses `sacct --json` (default data parser). If your site runs an older Slurm, the JSON schema in `src/job_stats/job_stats.rs` (`parse_job_record()`) will need adjustment for the older field layout.

`sreport` does not support JSON output in any current Slurm version. `account_usage` and `usage` parse its pipe-delimited (`-P`) output, which is stable across versions.

---

## Summary

For a minimal port, change these files:

1. `src/common/common.rs`: billing constants, fiscal year, default paths, cluster detection
2. `src/job_estimate/job_estimate.rs`: partition default, memory default, billing divisor
3. `src/job_stats/job_stats.rs`: billing divisor, Lighthouse exclusion
4. `src/maxwalltime/maxwalltime.rs`: epoch file convention, buffer/cap values
5. `src/modules_setup/modules_setup.rs`: example module path
6. `src/usage/usage.rs`: Lighthouse guard
7. `src/account_usage/account_usage.rs`: Lighthouse TRES swap

Everything else (table rendering, JSON output, concurrency, spinners, date validation) is institution-agnostic.

---

## 9. Color Palette

The truecolor palette in `src/common/common.rs` is derived from the [U-M Brand Color Guidelines](https://brand.umich.edu/design-resources/colors/). If your institution has its own brand colors, update the RGB values in the five `color_*()` functions:

| Function         | Role    | Default (U-M)                |
| ---------------- | ------- | ---------------------------- |
| `color_error()`  | Error   | `#c8352a` / red fallback     |
| `color_warning()`| Warning | `#e27328` / yellow fallback  |
| `color_success()`| Success | `#5ba84f` / green fallback   |
| `color_info()`   | Info    | `#4f95c9` / cyan fallback    |
| `color_dim()`    | Dim     | `#8e9094` / dimmed fallback  |

The ANSI-16 fallbacks (used when `$COLORTERM` is not `truecolor`/`24bit`) are standard terminal colors and generally do not need changing. The `colored` crate respects `NO_COLOR` automatically.
