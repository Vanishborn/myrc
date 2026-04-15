use std::env;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{Datelike, NaiveDate};
use colored::{ColoredString, Colorize};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Default per-subprocess timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Maximum concurrent Slurm subprocess calls (semaphore permits).
const MAX_CONCURRENT: usize = 12;

/// Terminal width threshold for adaptive table/key-value switching.
const NARROW_THRESHOLD: u16 = 105;

/// Default path for maintenance epoch files.
const DEFAULT_ETC_DIR: &str = "/sw/pkgs/arc/usertools/etc/";

/// Billing divisor before July 2021.
const BILLING_DIVISOR_OLD: u64 = 100_000;

/// Billing divisor from July 2021 onward.
const BILLING_DIVISOR_NEW: u64 = 10_000_000;

/// The cutoff date for the billing divisor change (2021-07-01).
const BILLING_CUTOFF: (i32, u32, u32) = (2021, 7, 1);

/// Whether the terminal advertises 24-bit truecolor support (checked once).
static TRUECOLOR: LazyLock<bool> = LazyLock::new(|| {
    env::var("COLORTERM")
        .map(|v| v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit"))
        .unwrap_or(false)
});

/// Error color: Tomato Jam `#c8352a`.
pub fn color_error(s: &str) -> ColoredString {
    if *TRUECOLOR {
        s.truecolor(200, 53, 42)
    } else {
        s.red()
    }
}

/// Warning color: Chocolate `#e27328`.
pub fn color_warning(s: &str) -> ColoredString {
    if *TRUECOLOR {
        s.truecolor(226, 115, 40)
    } else {
        s.yellow()
    }
}

/// Success color: Medium Jungle `#5ba84f`.
pub fn color_success(s: &str) -> ColoredString {
    if *TRUECOLOR {
        s.truecolor(91, 168, 79)
    } else {
        s.green()
    }
}

/// Info/spinner color: Blue Bell `#4f95c9`.
pub fn color_info(s: &str) -> ColoredString {
    if *TRUECOLOR {
        s.truecolor(79, 149, 201)
    } else {
        s.cyan()
    }
}

/// Dim/elapsed color: Rosy Granite `#8e9094`.
pub fn color_dim(s: &str) -> ColoredString {
    if *TRUECOLOR {
        s.truecolor(142, 144, 148)
    } else {
        s.dimmed()
    }
}

/// Returns the ANSI color name for spinner templates (not colorized strings).
/// In truecolor mode, returns an RGB escape; in ANSI mode, returns the name.
fn spinner_color_code(kind: SpinnerKind) -> String {
    if *TRUECOLOR {
        match kind {
            SpinnerKind::Total => "79,149,201".to_string(), // Blue Bell
            SpinnerKind::Success => "91,168,79".to_string(), // Medium Jungle
            SpinnerKind::Failed => "200,53,42".to_string(), // Tomato Jam
        }
    } else {
        match kind {
            SpinnerKind::Total => "cyan".to_string(),
            SpinnerKind::Success => "green".to_string(),
            SpinnerKind::Failed => "red".to_string(),
        }
    }
}

/// Returns the dim color code for spinner elapsed time.
fn spinner_dim_code() -> &'static str {
    if *TRUECOLOR {
        "142,144,148" // Rosy Granite
    } else {
        "dim"
    }
}

/// Error enum covering all failure modes in myrc.
#[derive(Debug, thiserror::Error)]
pub enum MyrcError {
    #[error("slurm command failed: {message}")]
    SlurmCmd {
        message: String,
        exit_code: ExitCode,
    },

    #[error("parse error: {0}")]
    Parse(String),

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl MyrcError {
    /// The exit code that should be used when this error terminates the process.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::SlurmCmd { exit_code, .. } => *exit_code,
            Self::Parse(_) => ExitCode::Failure,
            Self::Io(_) => ExitCode::Failure,
            Self::InvalidInput(_) => ExitCode::Failure,
        }
    }
}

/// Standard exit codes for myrc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Success = 0,
    Failure = 1,
    Usage = 2,
    ServiceUnavailable = 69,
    Config = 78,
    Interrupted = 130,
}

impl ExitCode {
    pub fn code(self) -> i32 {
        self as i32
    }
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(e: ExitCode) -> Self {
        std::process::ExitCode::from(e.code() as u8)
    }
}

/// Reads `$CLUSTER_NAME`, resolves epoch file paths via `$MYRC_ETC_DIR`.
#[derive(Debug, Clone)]
pub struct ClusterEnv {
    pub name: String,
    pub etc_dir: PathBuf,
}

impl ClusterEnv {
    /// Build from environment variables.
    ///
    /// Returns `MyrcError::InvalidInput` if `$CLUSTER_NAME` is unset or empty.
    pub fn from_env() -> Result<Self, MyrcError> {
        let name = env::var("CLUSTER_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| MyrcError::InvalidInput("$CLUSTER_NAME is not set".into()))?;
        let etc_dir = env::var("MYRC_ETC_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_ETC_DIR));
        tracing::debug!(cluster = %name, etc_dir = %etc_dir.display(), "detected cluster environment");
        Ok(Self { name, etc_dir })
    }

    /// Path to the maintenance epoch file for this cluster.
    pub fn epoch_path(&self) -> PathBuf {
        self.etc_dir
            .join(format!("{}_next_maintenance_epochtime", self.name))
    }

    /// Whether this cluster is Lighthouse (which has TRES quirks).
    pub fn is_lighthouse(&self) -> bool {
        self.name.eq_ignore_ascii_case("lighthouse")
    }
}

/// Controls whether output is human-readable tables or JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Table,
    Json,
}

impl OutputMode {
    pub fn is_json(self) -> bool {
        self == Self::Json
    }
}

/// Terminal width detection and narrow-mode check.
#[derive(Debug, Clone)]
pub struct TerminalInfo {
    pub width: u16,
}

impl TerminalInfo {
    /// Detect current terminal width. Falls back to 80 if not a TTY.
    pub fn detect() -> Self {
        let width = terminal_size::terminal_size()
            .map(|(w, _)| w.0)
            .unwrap_or(80);
        Self { width }
    }

    /// True when the terminal is narrower than 105 columns.
    pub fn is_narrow(&self) -> bool {
        self.width < NARROW_THRESHOLD
    }
}

/// Read timeout override from `$MYRC_SLURM_TIMEOUT`, falling back to 30s.
fn slurm_timeout() -> Duration {
    env::var("MYRC_SLURM_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
}

/// Run a single Slurm CLI command asynchronously with a 30s timeout.
///
/// On timeout the child process is killed and `MyrcError::SlurmCmd` is returned
/// with exit code 69 (service unavailable).
pub async fn slurm_cmd(args: &[impl AsRef<std::ffi::OsStr>]) -> Result<String, MyrcError> {
    let timeout = slurm_timeout();

    let cmd_str: Vec<String> = args
        .iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect();
    tracing::debug!(cmd = %cmd_str.join(" "), timeout_s = timeout.as_secs(), "spawning slurm command");

    let child = Command::new(&args[0])
        .args(&args[1..])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| MyrcError::SlurmCmd {
            message: format!("failed to spawn {:?}: {e}", args[0].as_ref()),
            exit_code: ExitCode::ServiceUnavailable,
        })?;

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                tracing::trace!(cmd = %cmd_str.join(" "), bytes = stdout.len(), "command succeeded");
                Ok(stdout)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(cmd = %cmd_str.join(" "), status = ?output.status, "command failed");
                Err(MyrcError::SlurmCmd {
                    message: stderr.trim().to_string(),
                    exit_code: ExitCode::Failure,
                })
            }
        }
        Ok(Err(io_err)) => Err(MyrcError::Io(io_err)),
        Err(_elapsed) => {
            // Timeout: the child is dropped here, which kills it automatically
            // when using tokio::process::Child (it sends SIGKILL on drop)
            tracing::error!(cmd = %cmd_str.join(" "), timeout_s = timeout.as_secs(), "command timed out");
            Err(MyrcError::SlurmCmd {
                message: format!("command timed out after {}s", timeout.as_secs()),
                exit_code: ExitCode::ServiceUnavailable,
            })
        }
    }
}

/// Run N Slurm commands concurrently (max 12 in-flight), returning results in
/// input order.
///
/// Uses a `tokio::sync::Semaphore` with 12 permits to cap concurrency.
pub async fn slurm_cmd_parallel(cmds: Vec<Vec<String>>) -> Result<Vec<String>, MyrcError> {
    let n = cmds.len();
    tracing::info!(
        count = n,
        max_concurrent = MAX_CONCURRENT,
        "starting parallel slurm commands"
    );
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let mut set = JoinSet::new();

    for (i, cmd) in cmds.into_iter().enumerate() {
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            (i, slurm_cmd(&cmd).await)
        });
    }

    let mut results = vec![String::new(); n];
    while let Some(res) = set.join_next().await {
        let (i, output) = res.map_err(|e| MyrcError::SlurmCmd {
            message: format!("task join error: {e}"),
            exit_code: ExitCode::Failure,
        })?;
        results[i] = output?;
    }
    Ok(results)
}

/// Parse pipe-delimited Slurm output (`sacctmgr -P`, `sreport -P`) into rows
/// of fields.
///
/// Each row is split on `|`. Leading/trailing blank lines are skipped. The
/// trailing `|` that Slurm appends to each line is handled.
pub fn parse_slurm_kv(output: &str) -> Vec<Vec<&str>> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let trimmed = line.strip_suffix('|').unwrap_or(line);
            trimmed.split('|').collect()
        })
        .collect()
}

/// Resolve a username: positional arg > `--user` flag > `$USER`.
pub fn resolve_user(positional: Option<&str>, flag: Option<&str>) -> Result<String, MyrcError> {
    let user = if let Some(u) = positional {
        u.to_string()
    } else if let Some(u) = flag {
        u.to_string()
    } else {
        env::var("USER").map_err(|_| {
            MyrcError::InvalidInput("could not determine username: $USER is not set".into())
        })?
    };
    tracing::debug!(user, "resolved user");
    Ok(user)
}

/// Validate that a Slurm account exists via `sacctmgr -p list Account`.
///
/// Returns `Ok(())` or `MyrcError::InvalidInput` with a clear message.
pub async fn validate_account(account: &str) -> Result<(), MyrcError> {
    tracing::debug!(account, "validating account");
    let output = slurm_cmd(&[
        "sacctmgr",
        "-n",
        "-p",
        "list",
        "account",
        account,
        "format=Account",
    ])
    .await?;

    let found = parse_slurm_kv(&output)
        .iter()
        .any(|row| row.first().is_some_and(|a| a.eq_ignore_ascii_case(account)));

    if found {
        Ok(())
    } else {
        Err(MyrcError::InvalidInput(format!(
            "account '{account}' not found"
        )))
    }
}

/// Jul–Jun fiscal year math.
#[derive(Debug, Clone)]
pub struct FiscalYear {
    /// Calendar year in which the fiscal year starts (July).
    pub start_year: i32,
}

impl FiscalYear {
    /// Fiscal year containing the given date.
    pub fn containing(date: NaiveDate) -> Self {
        let start_year = if date.month() >= 7 {
            date.year()
        } else {
            date.year() - 1
        };
        Self { start_year }
    }

    /// Current fiscal year.
    pub fn current() -> Self {
        Self::containing(chrono::Local::now().date_naive())
    }

    /// Previous fiscal year.
    pub fn previous() -> Self {
        Self {
            start_year: Self::current().start_year - 1,
        }
    }

    /// From an explicit year integer (the year July falls in).
    pub fn from_year(year: i32) -> Self {
        Self { start_year: year }
    }

    /// First day of the fiscal year (July 1).
    pub fn start_date(&self) -> NaiveDate {
        NaiveDate::from_ymd_opt(self.start_year, 7, 1).unwrap()
    }

    /// Last day of the fiscal year (June 30 of the following year).
    pub fn end_date(&self) -> NaiveDate {
        NaiveDate::from_ymd_opt(self.start_year + 1, 6, 30).unwrap()
    }

    /// Enumerate all 12 months as `(year, month)` tuples, July through June.
    pub fn months(&self) -> Vec<(i32, u32)> {
        let mut out = Vec::with_capacity(12);
        for m in 7..=12 {
            out.push((self.start_year, m));
        }
        for m in 1..=6 {
            out.push((self.start_year + 1, m));
        }
        out
    }
}

/// Validated start/end date range for `sreport`/`sacct` queries.
#[derive(Debug, Clone)]
pub struct DateRange {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl DateRange {
    /// Create from two `YYYY-MM` strings. End is set to the last day of that month.
    pub fn from_month_strings(start: &str, end: &str) -> Result<Self, MyrcError> {
        let s = parse_year_month(start)?;
        let e_first = parse_year_month(end)?;
        // End date = last day of the end month
        let e = last_day_of_month(e_first.year(), e_first.month());
        if s > e {
            return Err(MyrcError::InvalidInput(format!(
                "start date {start} is after end date {end}"
            )));
        }
        Ok(Self { start: s, end: e })
    }

    /// `sreport`-style start string: `YYYY-MM-DD`.
    pub fn start_str(&self) -> String {
        self.start.format("%Y-%m-%d").to_string()
    }

    /// `sreport`-style end string: `YYYY-MM-DD`.
    pub fn end_str(&self) -> String {
        self.end.format("%Y-%m-%d").to_string()
    }
}

/// Parse a `YYYY-MM` string into the first day of that month.
fn parse_year_month(s: &str) -> Result<NaiveDate, MyrcError> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 2 {
        return Err(MyrcError::InvalidInput(format!(
            "expected YYYY-MM, got '{s}'"
        )));
    }
    let year: i32 = parts[0]
        .parse()
        .map_err(|_| MyrcError::InvalidInput(format!("invalid year in '{s}'")))?;
    let month: u32 = parts[1]
        .parse()
        .map_err(|_| MyrcError::InvalidInput(format!("invalid month in '{s}'")))?;
    if !(1..=12).contains(&month) {
        return Err(MyrcError::InvalidInput(format!(
            "month out of range in '{s}'"
        )));
    }
    NaiveDate::from_ymd_opt(year, month, 1)
        .ok_or_else(|| MyrcError::InvalidInput(format!("invalid date '{s}'")))
}

/// Last day of a given month.
fn last_day_of_month(year: i32, month: u32) -> NaiveDate {
    if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap() - chrono::Duration::days(1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap() - chrono::Duration::days(1)
    }
}

/// Parse Slurm walltime formats into a `Duration`.
///
/// Handles: `DD-HH:MM:SS`, `HH:MM:SS`, `MM:SS`, `SS`.
pub fn parse_walltime(s: &str) -> Result<Duration, MyrcError> {
    let err = || MyrcError::Parse(format!("invalid walltime format: '{s}'"));

    // DD-HH:MM:SS
    if let Some((days_str, rest)) = s.split_once('-') {
        let days: u64 = days_str.parse().map_err(|_| err())?;
        let hms = parse_hms(rest).ok_or_else(err)?;
        return Ok(Duration::from_secs(days * 86400 + hms));
    }

    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        3 => {
            // HH:MM:SS
            let h: u64 = parts[0].parse().map_err(|_| err())?;
            let m: u64 = parts[1].parse().map_err(|_| err())?;
            let sec: u64 = parts[2].parse().map_err(|_| err())?;
            Ok(Duration::from_secs(h * 3600 + m * 60 + sec))
        }
        2 => {
            // MM:SS
            let m: u64 = parts[0].parse().map_err(|_| err())?;
            let sec: u64 = parts[1].parse().map_err(|_| err())?;
            Ok(Duration::from_secs(m * 60 + sec))
        }
        1 => {
            // SS
            let sec: u64 = parts[0].parse().map_err(|_| err())?;
            Ok(Duration::from_secs(sec))
        }
        _ => Err(err()),
    }
}

/// Parse `HH:MM:SS` into total seconds.
fn parse_hms(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let sec: u64 = parts[2].parse().ok()?;
    Some(h * 3600 + m * 60 + sec)
}

/// Parse a memory string with unit suffix into bytes.
///
/// Handles K/KB, M/MB, G/GB, T/TB (case-insensitive). IEC binary units assumed
/// (1G = 1 GiB = 1073741824 bytes), matching Slurm's convention.
/// Bare numbers without a suffix are multiplied by `default_unit` (1 for bytes,
/// `1 << 20` for MiB when parsing Slurm TRES fields).
pub fn parse_memory(s: &str, default_unit: u64) -> Result<u64, MyrcError> {
    let s = s.trim();
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix_ci("tb") {
        (n, 1u64 << 40)
    } else if let Some(n) = s.strip_suffix_ci("t") {
        (n, 1u64 << 40)
    } else if let Some(n) = s.strip_suffix_ci("gb") {
        (n, 1u64 << 30)
    } else if let Some(n) = s.strip_suffix_ci("g") {
        (n, 1u64 << 30)
    } else if let Some(n) = s.strip_suffix_ci("mb") {
        (n, 1u64 << 20)
    } else if let Some(n) = s.strip_suffix_ci("m") {
        (n, 1u64 << 20)
    } else if let Some(n) = s.strip_suffix_ci("kb") {
        (n, 1u64 << 10)
    } else if let Some(n) = s.strip_suffix_ci("k") {
        (n, 1u64 << 10)
    } else {
        (s, default_unit)
    };

    let num: f64 = num_str
        .parse()
        .map_err(|_| MyrcError::Parse(format!("invalid memory value: '{s}'")))?;
    Ok((num * multiplier as f64) as u64)
}

/// Case-insensitive suffix stripping helper.
trait StripSuffixCi {
    fn strip_suffix_ci(&self, suffix: &str) -> Option<&str>;
}

impl StripSuffixCi for str {
    fn strip_suffix_ci(&self, suffix: &str) -> Option<&str> {
        if self.len() >= suffix.len()
            && self[self.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        {
            Some(&self[..self.len() - suffix.len()])
        } else {
            None
        }
    }
}

/// Billing divisor for a given date.
///
/// Returns 100,000 before July 2021, 10,000,000 from July 2021 onward.
pub fn billing_divisor(date: &NaiveDate) -> u64 {
    let cutoff =
        NaiveDate::from_ymd_opt(BILLING_CUTOFF.0, BILLING_CUTOFF.1, BILLING_CUTOFF.2).unwrap();
    if *date < cutoff {
        BILLING_DIVISOR_OLD
    } else {
        BILLING_DIVISOR_NEW
    }
}

/// Compute cost in dollars: `billing_value × minutes / divisor`.
pub fn compute_cost(billing_value: f64, minutes: f64, divisor: u64) -> f64 {
    (billing_value * minutes) / divisor as f64
}

/// Format a dollar amount: `f64` → `"$1,234.56"`.
pub fn format_dollars(amount: f64) -> String {
    let negative = amount < 0.0;
    let abs = amount.abs();
    let cents = (abs * 100.0).round() as u64;
    let whole = cents / 100;
    let frac = cents % 100;
    let whole_str = format_with_thousands(whole);
    if negative {
        format!("-${whole_str}.{frac:02}")
    } else {
        format!("${whole_str}.{frac:02}")
    }
}

/// Insert thousands separators into an integer.
fn format_with_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result
}

/// Format a percentage: `f64` → `"87.3%"` (1 decimal).
pub fn format_percent(value: f64) -> String {
    format!("{:.1}%", value)
}

/// Format bytes into auto-scaled IEC unit: `"256 MiB"`, `"1.5 GiB"`, `"2.0 TiB"`.
pub fn format_memory(bytes: u64) -> String {
    const TIB: f64 = (1u64 << 40) as f64;
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    const KIB: f64 = (1u64 << 10) as f64;

    let b = bytes as f64;
    if b >= TIB {
        format!("{:.2} TiB", b / TIB)
    } else if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.0} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Format a duration as Slurm walltime: `DD-HH:MM:SS`.
pub fn format_walltime_slurm(d: Duration) -> String {
    let total_secs = d.as_secs();
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    format!("{days:02}-{hours:02}:{mins:02}:{secs:02}")
}

/// Format a duration as human-readable walltime: `HHH:MM:SS`.
pub fn format_walltime_human(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    format!("{hours:03}:{mins:02}:{secs:02}")
}

/// Column alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Column definition for `Table`.
#[derive(Debug, Clone)]
pub struct Column {
    pub header: String,
    pub align: Align,
}

/// Aligned columnar output with bold headers, optional totals row.
///
/// Adapts to terminal width. Right-aligns numbers, left-aligns strings.
/// Column separation: 2 spaces.
#[derive(Debug)]
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
    totals: Option<Vec<String>>,
}

impl Table {
    pub fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
            totals: None,
        }
    }

    /// Convenience: create from headers with auto-detected alignment.
    ///
    /// Headers starting with `$` or containing `%` or purely numeric fields
    /// default to right-aligned; everything else left-aligned.
    pub fn from_headers(headers: &[&str]) -> Self {
        let columns = headers
            .iter()
            .map(|h| Column {
                header: h.to_string(),
                align: Align::Left,
            })
            .collect();
        Self::new(columns)
    }

    /// Set a column to right-aligned.
    pub fn right_align(&mut self, index: usize) -> &mut Self {
        if let Some(col) = self.columns.get_mut(index) {
            col.align = Align::Right;
        }
        self
    }

    pub fn add_row(&mut self, row: Vec<String>) {
        self.rows.push(row);
    }

    pub fn set_totals(&mut self, totals: Vec<String>) {
        self.totals = Some(totals);
    }

    /// Render the table to a string with 2-space column separation.
    pub fn render(&self) -> String {
        if self.columns.is_empty() {
            return String::new();
        }

        // Compute column widths from headers + data + totals
        let num_cols = self.columns.len();
        let mut widths: Vec<usize> = self.columns.iter().map(|c| c.header.len()).collect();

        for row in &self.rows {
            for (i, cell) in row.iter().enumerate().take(num_cols) {
                widths[i] = widths[i].max(cell.len());
            }
        }
        if let Some(totals) = &self.totals {
            for (i, cell) in totals.iter().enumerate().take(num_cols) {
                widths[i] = widths[i].max(cell.len());
            }
        }

        let mut out = String::new();
        let sep = "  ";

        // Header row (bold)
        let header_parts: Vec<String> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| pad(&col.header, widths[i], col.align))
            .collect();
        out.push_str(&header_parts.join(sep).bold().to_string());
        out.push('\n');

        // Data rows
        for row in &self.rows {
            let parts: Vec<String> = self
                .columns
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                    pad(cell, widths[i], col.align)
                })
                .collect();
            out.push_str(&parts.join(sep));
            out.push('\n');
        }

        // Totals row (bold, preceded by divider)
        if let Some(totals) = &self.totals {
            let total_width: usize =
                widths.iter().sum::<usize>() + sep.len() * (num_cols.saturating_sub(1));
            out.push_str(&"─".repeat(total_width));
            out.push('\n');
            let parts: Vec<String> = self
                .columns
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let cell = totals.get(i).map(|s| s.as_str()).unwrap_or("");
                    pad(cell, widths[i], col.align)
                })
                .collect();
            out.push_str(&parts.join(sep).bold().to_string());
            out.push('\n');
        }

        out
    }
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// Pad a string to a given width with the specified alignment.
fn pad(s: &str, width: usize, align: Align) -> String {
    match align {
        Align::Left => format!("{s:<width$}"),
        Align::Right => format!("{s:>width$}"),
    }
}

/// Preconfigured `MultiProgress` with braille charset and staggered tick
/// intervals.
pub struct SpinnerGroup {
    pub mp: MultiProgress,
    spinners: Vec<ProgressBar>,
}

/// Spinner category for color coding.
pub enum SpinnerKind {
    Total,
    Success,
    Failed,
}

impl SpinnerGroup {
    pub fn new() -> Self {
        Self {
            mp: MultiProgress::new(),
            spinners: Vec::new(),
        }
    }

    /// Add a spinner with the given kind, prefix label, and unit name.
    ///
    /// The tick interval is staggered: 100 + (index × 20) ms.
    pub fn add(&mut self, kind: SpinnerKind, prefix: &str) -> &ProgressBar {
        let color = spinner_color_code(kind);
        let dim = spinner_dim_code();

        let template = format!(
            "{{spinner:.{color}}} {{prefix:.bold.white:<12}} {{msg:.bold.{color}:>6}} {{elapsed:.{dim}}}"
        );

        let style = ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template(&template)
            .unwrap();

        let pb = self.mp.add(ProgressBar::new_spinner());
        pb.set_style(style);
        pb.set_prefix(prefix.to_string());

        let idx = self.spinners.len();
        let tick_ms = 100 + (idx as u64 * 20);
        pb.enable_steady_tick(Duration::from_millis(tick_ms));

        self.spinners.push(pb);
        self.spinners.last().unwrap()
    }

    /// Finish and clear all spinners.
    pub fn finish(&self) {
        for pb in &self.spinners {
            pb.finish_and_clear();
        }
    }
}

impl Default for SpinnerGroup {
    fn default() -> Self {
        Self::new()
    }
}

/// Display a `"message [y/N]: "` prompt. Default is **no**.
///
/// Returns `true` only if the user types `y` or `Y`.
pub fn confirm_prompt(message: &str) -> Result<bool, MyrcError> {
    print!("{message} [y/N]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_memory -------------------------------------------------------

    #[test]
    fn parse_memory_gigabytes() {
        assert_eq!(parse_memory("10G", 1).unwrap(), 10 * (1 << 30));
        assert_eq!(parse_memory("10gb", 1).unwrap(), 10 * (1 << 30));
    }

    #[test]
    fn parse_memory_megabytes() {
        assert_eq!(parse_memory("512M", 1).unwrap(), 512 * (1 << 20));
        assert_eq!(parse_memory("512mb", 1).unwrap(), 512 * (1 << 20));
    }

    #[test]
    fn parse_memory_terabytes() {
        assert_eq!(parse_memory("1T", 1).unwrap(), 1 << 40);
    }

    #[test]
    fn parse_memory_kilobytes() {
        assert_eq!(parse_memory("1024K", 1).unwrap(), 1024 * (1 << 10));
    }

    #[test]
    fn parse_memory_bare_bytes() {
        assert_eq!(parse_memory("4096", 1).unwrap(), 4096);
    }

    #[test]
    fn parse_memory_bare_default_mib() {
        assert_eq!(parse_memory("1024", 1 << 20).unwrap(), 1024 * (1 << 20));
    }

    #[test]
    fn parse_memory_fractional() {
        assert_eq!(
            parse_memory("1.5G", 1).unwrap(),
            (1.5 * (1u64 << 30) as f64) as u64
        );
    }

    #[test]
    fn parse_memory_invalid() {
        assert!(parse_memory("abc", 1).is_err());
    }

    // -- parse_walltime -----------------------------------------------------

    #[test]
    fn parse_walltime_dd_hh_mm_ss() {
        let d = parse_walltime("1-12:30:00").unwrap();
        assert_eq!(d.as_secs(), 86400 + 12 * 3600 + 30 * 60);
    }

    #[test]
    fn parse_walltime_hh_mm_ss() {
        let d = parse_walltime("02:30:00").unwrap();
        assert_eq!(d.as_secs(), 2 * 3600 + 30 * 60);
    }

    #[test]
    fn parse_walltime_mm_ss() {
        let d = parse_walltime("45:30").unwrap();
        assert_eq!(d.as_secs(), 45 * 60 + 30);
    }

    #[test]
    fn parse_walltime_ss() {
        let d = parse_walltime("90").unwrap();
        assert_eq!(d.as_secs(), 90);
    }

    #[test]
    fn parse_walltime_invalid() {
        assert!(parse_walltime("abc").is_err());
    }

    // -- format_dollars -----------------------------------------------------

    #[test]
    fn format_dollars_basic() {
        assert_eq!(format_dollars(1234.56), "$1,234.56");
    }

    #[test]
    fn format_dollars_small() {
        assert_eq!(format_dollars(0.03), "$0.03");
    }

    #[test]
    fn format_dollars_zero() {
        assert_eq!(format_dollars(0.0), "$0.00");
    }

    #[test]
    fn format_dollars_large() {
        assert_eq!(format_dollars(1_000_000.0), "$1,000,000.00");
    }

    // -- format_percent -----------------------------------------------------

    #[test]
    fn format_percent_basic() {
        assert_eq!(format_percent(87.3), "87.3%");
    }

    #[test]
    fn format_percent_zero() {
        assert_eq!(format_percent(0.0), "0.0%");
    }

    #[test]
    fn format_percent_hundred() {
        assert_eq!(format_percent(100.0), "100.0%");
    }

    // -- format_memory ------------------------------------------------------

    #[test]
    fn format_memory_tib() {
        assert_eq!(format_memory(2 * (1u64 << 40)), "2.00 TiB");
    }

    #[test]
    fn format_memory_gib() {
        let bytes = (1.5 * (1u64 << 30) as f64) as u64;
        assert_eq!(format_memory(bytes), "1.50 GiB");
    }

    #[test]
    fn format_memory_mib() {
        assert_eq!(format_memory(256 * (1 << 20)), "256 MiB");
    }

    // -- format_walltime ----------------------------------------------------

    #[test]
    fn format_walltime_slurm_basic() {
        let d = Duration::from_secs(86400 + 12 * 3600 + 30 * 60);
        assert_eq!(format_walltime_slurm(d), "01-12:30:00");
    }

    #[test]
    fn format_walltime_human_basic() {
        let d = Duration::from_secs(36 * 3600 + 30 * 60);
        assert_eq!(format_walltime_human(d), "036:30:00");
    }

    // -- billing_divisor ----------------------------------------------------

    #[test]
    fn billing_divisor_old() {
        let date = NaiveDate::from_ymd_opt(2021, 6, 30).unwrap();
        assert_eq!(billing_divisor(&date), 100_000);
    }

    #[test]
    fn billing_divisor_new() {
        let date = NaiveDate::from_ymd_opt(2021, 7, 1).unwrap();
        assert_eq!(billing_divisor(&date), 10_000_000);
    }

    // -- compute_cost -------------------------------------------------------

    #[test]
    fn compute_cost_basic() {
        let cost = compute_cost(36.0, 60.0, 10_000_000);
        assert!((cost - 0.000216).abs() < 1e-9);
    }

    // -- parse_slurm_kv -----------------------------------------------------

    #[test]
    fn parse_slurm_kv_basic() {
        let output = "a|b|c|\nd|e|f|\n";
        let rows = parse_slurm_kv(output);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["a", "b", "c"]);
        assert_eq!(rows[1], vec!["d", "e", "f"]);
    }

    #[test]
    fn parse_slurm_kv_no_trailing_pipe() {
        let output = "a|b|c\n";
        let rows = parse_slurm_kv(output);
        assert_eq!(rows[0], vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_slurm_kv_skips_blank() {
        let output = "\na|b|\n\n";
        let rows = parse_slurm_kv(output);
        assert_eq!(rows.len(), 1);
    }

    // -- resolve_user -------------------------------------------------------

    #[test]
    fn resolve_user_positional_wins() {
        let u = resolve_user(Some("alice"), Some("bob")).unwrap();
        assert_eq!(u, "alice");
    }

    #[test]
    fn resolve_user_flag_fallback() {
        let u = resolve_user(None, Some("bob")).unwrap();
        assert_eq!(u, "bob");
    }

    // -- FiscalYear ---------------------------------------------------------

    #[test]
    fn fiscal_year_months() {
        let fy = FiscalYear::from_year(2025);
        let months = fy.months();
        assert_eq!(months.len(), 12);
        assert_eq!(months[0], (2025, 7));
        assert_eq!(months[11], (2026, 6));
    }

    #[test]
    fn fiscal_year_containing_july() {
        let date = NaiveDate::from_ymd_opt(2025, 7, 15).unwrap();
        let fy = FiscalYear::containing(date);
        assert_eq!(fy.start_year, 2025);
    }

    #[test]
    fn fiscal_year_containing_march() {
        let date = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let fy = FiscalYear::containing(date);
        assert_eq!(fy.start_year, 2025);
    }

    // -- DateRange ----------------------------------------------------------

    #[test]
    fn date_range_basic() {
        let dr = DateRange::from_month_strings("2025-07", "2026-06").unwrap();
        assert_eq!(dr.start_str(), "2025-07-01");
        assert_eq!(dr.end_str(), "2026-06-30");
    }

    #[test]
    fn date_range_invalid_order() {
        assert!(DateRange::from_month_strings("2026-06", "2025-07").is_err());
    }

    // -- Table --------------------------------------------------------------

    #[test]
    fn table_render_basic() {
        let mut table = Table::from_headers(&["Name", "Value"]);
        table.right_align(1);
        table.add_row(vec!["alice".into(), "$100.00".into()]);
        table.add_row(vec!["bob".into(), "$200.00".into()]);
        table.set_totals(vec!["Total".into(), "$300.00".into()]);
        let rendered = table.render();
        assert!(rendered.contains("alice"));
        assert!(rendered.contains("$300.00"));
        assert!(rendered.contains("─"));
    }
}
