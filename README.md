# myrc

[![DOI](https://zenodo.org/badge/1209739097.svg)](https://doi.org/10.5281/zenodo.19587073)

A unified CLI for managing resources on University of Michigan HPC clusters.

Successor to [slurm-usertools](https://bitbucket.org/umarcts/slurm-usertools). A single binary replaces the collection of Python, Perl, and shell scripts. Written in Rust.

## Install

Pre-built Linux binaries for x86_64, aarch64, and ppc64le are available on the [Releases](https://github.com/Vanishborn/myrc/releases/latest) page.

```bash
# Or build from source
make install
```

## Usage

```bash
myrc <COMMAND> [OPTIONS]
```

**Global flags:**

| Flag            | Description                                               |
| --------------- | --------------------------------------------------------- |
| `--json`        | Output JSON instead of table format                       |
| `--color`       | Color mode: `auto` (default), `always`, `never`           |
| `-v, --verbose` | Increase verbosity (`-v` info, `-vv` debug, `-vvv` trace) |

Commands that accept `-u, --user` default to `$USER` when omitted.

## Commands

### `myrc accounts [USER]`

List Slurm resource accounts and their limits.

| Flag         | Description                                |
| ------------ | ------------------------------------------ |
| `-u, --user` | User to query (also accepted positionally) |

### `myrc account usage -a ACCOUNT`

Report monthly billing cost per-user for an account.

| Flag                 | Description                                                |
| -------------------- | ---------------------------------------------------------- |
| `-a, --account`      | Account to report *(required)*                             |
| `-y, --year`         | Fiscal year: integer, `this`, or `last`                    |
| `-s, --start`        | Start month `YYYY-MM` (requires `--end`)                   |
| `-e, --end`          | End month `YYYY-MM` (requires `--start`)                   |
| `-t, --type`         | TRES type: `billing`, `cpu`, or `gpu` (default: `billing`) |
| `-p, --percentage`   | Show per-user percentage columns                           |
| `--sort-by-total`    | Sort users by total across range *(default)*               |
| `--sort-by-current`  | Sort users by current (latest) month                       |
| `--sort-by-previous` | Sort users by previous (second-to-last) month              |
| `--sort-by-user`     | Sort users alphabetically by login                         |

### `myrc account running -a ACCOUNT`

Show cumulative resources of running jobs for an account.

| Flag            | Description                                                |
| --------------- | ---------------------------------------------------------- |
| `-a, --account` | Account to query *(required)*                              |
| `-d, --detail`  | Per-job breakdown: user, jobid, nodes, cores, GPUs, memory |

### `myrc usage [USER]`

Show billing usage (as dollars) across all accounts for a month.

| Flag          | Description                                |
| ------------- | ------------------------------------------ |
| `-u, --user`  | User to query (also accepted positionally) |
| `-y, --year`  | Year to query                              |
| `-m, --month` | Month to query (1-12)                      |

### `myrc job estimate`

Estimate the dollar cost of a hypothetical job.

| Flag              | Description                                                          |
| ----------------- | -------------------------------------------------------------------- |
| `-p, --partition` | Partition name (default: `standard`)                                 |
| `-c, --cores`     | Total cores (default: `1`)                                           |
| `-g, --gpus`      | Total GPUs (default: `0`)                                            |
| `-n, --nodes`     | Total nodes (default: `1`)                                           |
| `-m, --memory`    | Memory with unit, e.g. `10g`, `768mb` (default: `768mb`)             |
| `-t, --time`      | Walltime `DD-HH:MM:SS`, `HH:MM:SS`, or `MM:SS` (default: `01:00:00`) |

### `myrc job header`

Print Slurm job environment header (for use inside job scripts). No flags.

### `myrc job list`

List and filter a user's jobs.

| Flag            | Description                                                                                 |
| --------------- | ------------------------------------------------------------------------------------------- |
| `-u, --user`    | User to query                                                                               |
| `-y, --year`    | Filter by year                                                                              |
| `-m, --month`   | Filter by month (1-12)                                                                      |
| `-d, --day`     | Filter by day (1-31)                                                                        |
| `-t, --type`    | Filter by state: `completed`, `failed`, `timeout`, `running`, `pending`, `cancelled`, `oom` |
| `-a, --account` | Filter by account                                                                           |
| `-n, --limit`   | Max rows to show (default: `25`)                                                            |
| `--sort-by`     | Sort key: `submit`, `start`, `end`, `id` (default: `submit`)                                |
| `--reverse`     | Reverse sort order                                                                          |

### `myrc job stats [JOBID]`

Show detailed job statistics and efficiency.

| Flag    | Description                          |
| ------- | ------------------------------------ |
| `JOBID` | Job ID (defaults to most recent job) |
| `--raw` | Output full job record as JSON       |

### `myrc maxwalltime`

Calculate maximum walltime before next maintenance window.

| Flag                 | Description                                     |
| -------------------- | ----------------------------------------------- |
| `-S, --slurm-format` | Print only the walltime in `DD-HH:MM:SS` format |

### `myrc modules setup`

Set up a personal Lmod module directory.

| Flag        | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

### `myrc sstate`

Per-node cluster resource dashboard (CPU, memory, GPU allocation and availability).

| Flag              | Description                                                         |
| ----------------- | ------------------------------------------------------------------- |
| `-p, --partition` | Filter to a specific partition                                      |
| `--raw`           | Show raw availability (disable bottleneck rule and state filtering) |

### `myrc completions <SHELL>`

Generate shell completion script. Supported shells: `bash`, `zsh`, `fish`.

**Bash:**

```bash
mkdir -p ~/.local/share/bash-completion/completions
myrc completions bash > ~/.local/share/bash-completion/completions/myrc
```

**Zsh:**

```bash
mkdir -p ~/.zsh/completions
myrc completions zsh > ~/.zsh/completions/_myrc
```

Then add the following to `~/.zshrc` **before** `compinit`:

```bash
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```

**Fish:**

```bash
mkdir -p ~/.config/fish/completions
myrc completions fish > ~/.config/fish/completions/myrc.fish
```

Restart the shell after installing completions for them to take effect.

## Build

Requires Rust 1.85+ (edition 2024).

```bash
make build            # Debug build
make build-release    # Release build (native)
make test             # Tests + clippy
make check            # Format check + clippy (CI-style)
make release          # Cross-compile all Linux targets
make install          # Install to $CARGO_HOME/bin
make install PREFIX=<path>
                      # Full install: binary + man pages + completions
make clean            # Remove build artifacts
make help             # Show all targets
```

Cross-compilation requires [zig](https://ziglang.org/) and [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild).

### Supported Platforms

| Target                          | Min glibc |
| ------------------------------- | --------- |
| `x86_64-unknown-linux-gnu`      | 2.17      |
| `aarch64-unknown-linux-gnu`     | 2.17      |
| `powerpc64le-unknown-linux-gnu` | 2.19      |

## Compatibility

- Slurm 25.11+ (`sacct --json`)
- RHEL/Rocky 7+ (glibc 2.17+)

See [docs/adapting.md](docs/adapting.md) for notes on adapting to other institutions.

## Color

Colored output is enabled automatically for interactive terminals and disabled when piping or redirecting.

Override with the `--color` flag:

```bash
myrc --color=never sstate   # no ANSI escapes
myrc --color=always sstate  # force color even when piped
```

The tool also respects the [`NO_COLOR`](https://no-color.org/) convention.

Set `NO_COLOR=1` in your environment to suppress color globally:

```bash
export NO_COLOR=1
```

## License

GPL-3.0-or-later.
