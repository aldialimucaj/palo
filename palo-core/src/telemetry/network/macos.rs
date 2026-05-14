use std::collections::BTreeSet;
use std::io;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tracing::debug;

use super::NetworkUsage;

const NETTOP_TIMEOUT: Duration = Duration::from_millis(1500);
const NETTOP_POLL_INTERVAL: Duration = Duration::from_millis(25);
const NETTOP_COLUMNS: &str = "bytes_in,bytes_out";

#[derive(Debug)]
pub(super) struct Collector {
    timeout: Duration,
}

impl Collector {
    pub fn new() -> Self {
        Self {
            timeout: NETTOP_TIMEOUT,
        }
    }

    pub fn sample_process_group(&mut self, root_pid: u32, pids: &[u32]) -> Option<NetworkUsage> {
        let target_pids = normalize_pids(root_pid, pids);

        let output = match run_nettop(&target_pids, self.timeout) {
            Ok(output) => output,
            Err(NettopError::Spawn(error)) => {
                debug!(
                    root_pid,
                    pids = ?target_pids,
                    error = %error,
                    "failed to start nettop while collecting process network telemetry"
                );
                return None;
            }
            Err(NettopError::Wait(error)) => {
                debug!(
                    root_pid,
                    pids = ?target_pids,
                    error = %error,
                    "failed to wait for nettop while collecting process network telemetry"
                );
                return None;
            }
            Err(NettopError::TimedOut) => {
                debug!(
                    root_pid,
                    pids = ?target_pids,
                    timeout_ms = self.timeout.as_millis(),
                    "nettop timed out while collecting process network telemetry"
                );
                return None;
            }
            Err(NettopError::Failed { status, stderr }) => {
                debug!(
                    root_pid,
                    pids = ?target_pids,
                    ?status,
                    stderr = %stderr.trim(),
                    "nettop did not return process network telemetry"
                );
                return None;
            }
        };

        let usage = parse_nettop_usage(&output, &target_pids);
        if usage.is_none() {
            debug!(
                root_pid,
                pids = ?target_pids,
                "nettop output did not contain parsable process network telemetry"
            );
        }
        usage
    }
}

impl Default for Collector {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
enum NettopError {
    Spawn(io::Error),
    Wait(io::Error),
    TimedOut,
    Failed { status: Option<i32>, stderr: String },
}

#[derive(Debug, Clone, Copy)]
struct ByteColumns {
    pid: Option<usize>,
    rx: usize,
    tx: usize,
}

fn normalize_pids(root_pid: u32, pids: &[u32]) -> Vec<u32> {
    let mut target_pids = BTreeSet::new();
    target_pids.insert(root_pid);
    target_pids.extend(pids.iter().copied().filter(|pid| *pid != 0));
    target_pids.into_iter().collect()
}

fn run_nettop(pids: &[u32], timeout: Duration) -> Result<String, NettopError> {
    let mut command = Command::new("nettop");
    command
        .args(["-n", "-P", "-x", "-L", "1", "-J", NETTOP_COLUMNS])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for pid in pids {
        command.arg("-p").arg(pid.to_string());
    }

    let mut child = command.spawn().map_err(NettopError::Spawn)?;
    let started = Instant::now();

    loop {
        match child.try_wait().map_err(NettopError::Wait)? {
            Some(_) => {
                let output = child.wait_with_output().map_err(NettopError::Wait)?;
                if output.status.success() {
                    return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
                }

                return Err(NettopError::Failed {
                    status: output.status.code(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(NettopError::TimedOut);
            }
            None => thread::sleep(NETTOP_POLL_INTERVAL),
        }
    }
}

fn parse_nettop_usage(output: &str, pids: &[u32]) -> Option<NetworkUsage> {
    let target_pids = pids.iter().copied().collect::<BTreeSet<_>>();
    let mut columns = None;
    let mut usage = NetworkUsage {
        rx_bytes: 0,
        tx_bytes: 0,
    };
    let mut matched = false;

    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let fields = split_record(line);
        if fields.is_empty() {
            continue;
        }

        if let Some(parsed_columns) = parse_byte_columns(&fields) {
            columns = Some(parsed_columns);
            continue;
        }

        if let Some(parsed_usage) = columns
            .and_then(|byte_columns| parse_columnar_usage(&fields, byte_columns, &target_pids))
            .or_else(|| parse_labeled_usage(&fields, &target_pids))
        {
            usage.rx_bytes = usage.rx_bytes.saturating_add(parsed_usage.rx_bytes);
            usage.tx_bytes = usage.tx_bytes.saturating_add(parsed_usage.tx_bytes);
            matched = true;
        }
    }

    matched.then_some(usage)
}

fn split_record(line: &str) -> Vec<String> {
    if line.contains(',') {
        split_csv_record(line)
    } else {
        line.split_whitespace()
            .map(|field| trim_field(field).to_owned())
            .filter(|field| !field.is_empty())
            .collect()
    }
}

fn split_csv_record(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(trim_field(&current).to_owned());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    fields.push(trim_field(&current).to_owned());
    fields
}

fn parse_byte_columns(fields: &[String]) -> Option<ByteColumns> {
    let mut pid = None;
    let mut rx = None;
    let mut tx = None;

    for (index, field) in fields.iter().enumerate() {
        match normalize_header(field).as_str() {
            "pid" | "processid" => pid = Some(index),
            "bytesin" | "rxbytes" | "receivedbytes" | "inputbytes" => rx = Some(index),
            "bytesout" | "txbytes" | "sentbytes" | "outputbytes" => tx = Some(index),
            _ => {}
        }
    }

    Some(ByteColumns {
        pid,
        rx: rx?,
        tx: tx?,
    })
}

fn parse_columnar_usage(
    fields: &[String],
    columns: ByteColumns,
    target_pids: &BTreeSet<u32>,
) -> Option<NetworkUsage> {
    let max_column = columns.rx.max(columns.tx).max(columns.pid.unwrap_or(0));
    if fields.len() <= max_column || !record_matches_target_pid(fields, target_pids, columns.pid) {
        return None;
    }

    Some(NetworkUsage {
        rx_bytes: parse_byte_value(&fields[columns.rx])?,
        tx_bytes: parse_byte_value(&fields[columns.tx])?,
    })
}

fn parse_labeled_usage(fields: &[String], target_pids: &BTreeSet<u32>) -> Option<NetworkUsage> {
    if !record_matches_target_pid(fields, target_pids, None) {
        return None;
    }

    let mut rx = None;
    let mut tx = None;

    for field in fields {
        let Some((label, value)) = field.split_once('=') else {
            continue;
        };

        match normalize_header(label).as_str() {
            "bytesin" | "rxbytes" | "receivedbytes" | "inputbytes" => rx = parse_byte_value(value),
            "bytesout" | "txbytes" | "sentbytes" | "outputbytes" => tx = parse_byte_value(value),
            _ => {}
        }
    }

    Some(NetworkUsage {
        rx_bytes: rx?,
        tx_bytes: tx?,
    })
}

fn record_matches_target_pid(
    fields: &[String],
    target_pids: &BTreeSet<u32>,
    pid_column: Option<usize>,
) -> bool {
    if let Some(pid_column) = pid_column {
        return fields
            .get(pid_column)
            .and_then(|field| parse_pid_value(field))
            .is_some_and(|pid| target_pids.contains(&pid));
    }

    fields
        .iter()
        .take(3)
        .filter_map(|field| parse_process_field_pid(field))
        .any(|pid| target_pids.contains(&pid))
        || fields
            .iter()
            .filter_map(|field| parse_keyed_pid(field))
            .any(|pid| target_pids.contains(&pid))
}

fn parse_pid_value(field: &str) -> Option<u32> {
    parse_keyed_pid(field).or_else(|| {
        let trimmed = trim_field(field);
        trimmed
            .chars()
            .all(|ch| ch.is_ascii_digit())
            .then(|| trimmed.parse::<u32>().ok())
            .flatten()
    })
}

fn parse_process_field_pid(field: &str) -> Option<u32> {
    let trimmed = trim_field(field);
    if let Some(pid) = parse_pid_value(trimmed) {
        return Some(pid);
    }

    if looks_like_network_endpoint(trimmed) {
        return None;
    }

    let end = trimmed
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_ascii_digit())
        .map(|(index, ch)| index + ch.len_utf8())?;
    let start = trimmed[..end]
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_ascii_digit())
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(0);

    if start == 0 {
        return None;
    }

    let separator = trimmed[..start].chars().last()?;
    matches!(separator, '.' | '/' | '(')
        .then(|| trimmed[start..end].parse::<u32>().ok())
        .flatten()
}

fn parse_keyed_pid(field: &str) -> Option<u32> {
    for key in ["pid=", "pid:"] {
        let Some(start) = field.find(key).map(|index| index + key.len()) else {
            continue;
        };
        let digits = field[start..]
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if let Ok(pid) = digits.parse::<u32>() {
            return Some(pid);
        }
    }

    None
}

fn parse_byte_value(field: &str) -> Option<u64> {
    let value = trim_field(field);
    let value = value
        .split_once('=')
        .map(|(_, value)| value)
        .unwrap_or(value)
        .trim();
    let value = value.replace(['_', ','], "");

    let number_end = value
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit() || *ch == '.')
        .map(|(index, ch)| index + ch.len_utf8())
        .last()?;
    let number = value[..number_end].parse::<f64>().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }

    let unit = normalize_header(&value[number_end..]);
    let multiplier = match unit.as_str() {
        "" | "b" | "byte" | "bytes" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };

    Some((number * multiplier).round() as u64)
}

fn normalize_header(value: &str) -> String {
    trim_field(value)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn trim_field(value: &str) -> &str {
    value.trim().trim_matches(|ch: char| {
        ch == '"' || ch == '\'' || ch == '[' || ch == ']' || ch == '(' || ch == ')' || ch == ','
    })
}

fn looks_like_network_endpoint(value: &str) -> bool {
    value.contains(':')
        || value
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | '[' | ']'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_csv_nettop_process_rows() {
        let output = r#"
process,bytes_in,bytes_out
node.123,1024,2048
other.999,9,9
"#;

        assert_eq!(
            parse_nettop_usage(output, &[123]),
            Some(NetworkUsage {
                rx_bytes: 1024,
                tx_bytes: 2048
            })
        );
    }

    #[test]
    fn parses_csv_rows_with_explicit_pid_column() {
        let output = r#"
pid,bytes_in,bytes_out
123,4096,8192
999,1,2
"#;

        assert_eq!(
            parse_nettop_usage(output, &[123]),
            Some(NetworkUsage {
                rx_bytes: 4096,
                tx_bytes: 8192
            })
        );
    }

    #[test]
    fn sums_multiple_matching_process_rows() {
        let output = r#"
process,bytes_in,bytes_out
api.123,100,200
worker.124,300,400
other.999,900,900
"#;

        assert_eq!(
            parse_nettop_usage(output, &[123, 124]),
            Some(NetworkUsage {
                rx_bytes: 400,
                tx_bytes: 600
            })
        );
    }

    #[test]
    fn parses_table_rows_with_headers() {
        let output = r#"
process         bytes_in     bytes_out
palo.123        1536         3072
other.999       1            2
"#;

        assert_eq!(
            parse_nettop_usage(output, &[123]),
            Some(NetworkUsage {
                rx_bytes: 1536,
                tx_bytes: 3072
            })
        );
    }

    #[test]
    fn parses_labeled_table_rows() {
        let output = "process=palo pid=123 bytes_in=1KiB bytes_out=2KiB";

        assert_eq!(
            parse_nettop_usage(output, &[123]),
            Some(NetworkUsage {
                rx_bytes: 1024,
                tx_bytes: 2048
            })
        );
    }

    #[test]
    fn rejects_partial_pid_matches() {
        let output = r#"
process,bytes_in,bytes_out
node.1234,1024,2048
"#;

        assert_eq!(parse_nettop_usage(output, &[123]), None);
    }

    #[test]
    fn rejects_endpoint_pid_lookalikes_without_process_pid() {
        let output = r#"
process,bytes_in,bytes_out
1.2.3.4:443,1024,2048
"#;

        assert_eq!(parse_nettop_usage(output, &[443]), None);
    }

    #[test]
    fn returns_none_without_pid_attribution() {
        let output = r#"
bytes_in,bytes_out
1024,2048
"#;

        assert_eq!(parse_nettop_usage(output, &[123]), None);
    }
}
