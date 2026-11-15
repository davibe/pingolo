use std::collections::VecDeque;
use std::net::IpAddr;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

pub struct Reply {
    pub sequence: u16,
    pub milliseconds: Option<f64>,
}

pub fn parse(line: &str) -> Option<Reply> {
    let (_, suffix) = line.split_once("icmp_seq")?;
    let sequence = suffix
        .trim_start_matches(['=', ' '])
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let milliseconds = line
        .split_once("time=")
        .or_else(|| line.split_once("time<"))
        .and_then(|(_, value)| value.split_whitespace().next()?.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0);

    if milliseconds.is_some() || line.contains("Request timeout") || line.contains("no answer yet")
    {
        Some(Reply {
            sequence,
            milliseconds,
        })
    } else {
        None
    }
}

pub fn spawn(address: IpAddr, interval: f64, timeout: f64) -> Result<Child> {
    let program = if cfg!(target_os = "macos") && address.is_ipv6() {
        "ping6"
    } else {
        "ping"
    };
    let mut command = Command::new(program);
    command
        .env("LC_ALL", "C")
        .args(["-n", "-i", &interval.to_string()]);

    if cfg!(target_os = "macos") {
        command.args(["-W", &(timeout * 1000.0).ceil().to_string()]);
    } else {
        command.args(["-O", "-W", &timeout.to_string()]);
        if address.is_ipv6() {
            command.arg("-6");
        }
    }

    command
        .arg(address.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Could not start the system ping command")
}

pub struct Sample {
    pub sequence: i64,
    pub sent: Instant,
    pub milliseconds: Option<f64>,
}

#[derive(Default)]
pub struct History {
    pub samples: VecDeque<Sample>,
}

pub struct Stats {
    pub last: Option<f64>,
    pub min: f64,
    pub max: f64,
    pub average: f64,
    pub received: usize,
    pub lost: usize,
}

impl History {
    pub fn record(&mut self, reply: Reply, now: Instant, interval: Duration) {
        // Sequence numbers wrap at 65536; replies can arrive out of order.
        let sequence = match self.samples.back() {
            Some(last) => {
                last.sequence + i64::from(reply.sequence.wrapping_sub(last.sequence as u16) as i16)
            }
            None => i64::from(reply.sequence),
        };
        let sent = now
            - reply
                .milliseconds
                .map_or(interval, |ms| Duration::from_secs_f64(ms / 1000.0));

        if let Some(sample) = self
            .samples
            .iter_mut()
            .rev()
            .find(|sample| sample.sequence == sequence)
        {
            if let Some(milliseconds) = reply.milliseconds {
                sample.milliseconds = Some(milliseconds);
                sample.sent = sent;
            }
            return;
        }

        let index = self
            .samples
            .partition_point(|sample| sample.sequence < sequence);
        self.samples.insert(
            index,
            Sample {
                sequence,
                sent,
                milliseconds: reply.milliseconds,
            },
        );
    }

    pub fn prune(&mut self, now: Instant, window: Duration) {
        self.samples
            .retain(|sample| now.saturating_duration_since(sample.sent) <= window);
    }

    pub fn stats(&self, now: Instant, timeout: Duration) -> Stats {
        let mut stats = Stats {
            last: None,
            min: f64::INFINITY,
            max: 0.0,
            average: 0.0,
            received: 0,
            lost: 0,
        };
        for sample in &self.samples {
            match sample.milliseconds {
                Some(ms) => {
                    stats.last = Some(ms);
                    stats.min = stats.min.min(ms);
                    stats.max = stats.max.max(ms);
                    stats.average += ms;
                    stats.received += 1;
                }
                None if now.saturating_duration_since(sample.sent) >= timeout => stats.lost += 1,
                None => {}
            }
        }
        if stats.received > 0 {
            stats.average /= stats.received as f64;
        } else {
            stats.min = 0.0;
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macos_linux_ipv6_and_timeouts() {
        assert_eq!(
            parse("64 bytes from 192.168.1.1: icmp_seq=7 ttl=64 time=4.123 ms")
                .unwrap()
                .milliseconds,
            Some(4.123)
        );
        assert_eq!(
            parse("64 bytes from ::1: icmp_seq=1 hlim=64 time=0.044 ms")
                .unwrap()
                .sequence,
            1
        );
        assert_eq!(
            parse("64 bytes from 1.1.1.1: icmp_seq=3 ttl=56 time<1 ms")
                .unwrap()
                .milliseconds,
            Some(1.0)
        );
        assert_eq!(
            parse("Request timeout for icmp_seq 42").unwrap().sequence,
            42
        );
        assert!(
            parse("no answer yet for icmp_seq=9")
                .unwrap()
                .milliseconds
                .is_none()
        );
        assert!(parse("PING 192.168.1.1 (192.168.1.1): 56 data bytes").is_none());
    }

    #[test]
    fn late_replies_replace_timeouts_and_wrap_sequences() {
        let now = Instant::now();
        let interval = Duration::from_millis(100);
        let mut history = History::default();
        for (sequence, milliseconds) in [
            (65535, None),
            (0, Some(5.0)),
            (65535, Some(150.0)),
            (65535, None),
        ] {
            history.record(
                Reply {
                    sequence,
                    milliseconds,
                },
                now,
                interval,
            );
        }
        assert_eq!(history.samples.len(), 2);
        assert_eq!(history.samples[0].milliseconds, Some(150.0));
        assert_eq!(history.samples[1].sequence, 65536);
        let stats = history.stats(now + Duration::from_secs(2), Duration::from_secs(1));
        assert_eq!((stats.received, stats.lost), (2, 0));
    }
}
