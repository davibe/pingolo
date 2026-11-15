mod graph;
mod ping;

use std::io::{self, IsTerminal};
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::time;

use graph::Terminal;
use ping::History;

#[derive(Parser)]
#[command(
    version,
    about = "Ping with a smooth pixel graph in Kitty (also inside tmux)"
)]
struct Args {
    /// IPv4 or IPv6 address to ping
    address: IpAddr,
    /// Seconds between ICMP requests
    #[arg(short = 'i', long, default_value = "0.1", value_parser = positive)]
    interval: f64,
    /// Seconds displayed in the graph
    #[arg(short = 'w', long, default_value = "20", value_parser = positive)]
    window: f64,
    /// Maximum graph frames per second, independent of the ping rate
    #[arg(long, default_value = "10")]
    fps: NonZeroU32,
    /// Image resolution relative to terminal pixels (1 = native, 0.5 = half)
    #[arg(long, default_value = "0.5", value_parser = positive)]
    scale: f64,
    /// Seconds to wait before a missing reply counts as lost
    #[arg(long, default_value = "1", value_parser = positive)]
    timeout: f64,
    /// Fix the vertical scale, in milliseconds (otherwise automatic)
    #[arg(long, value_parser = positive)]
    ymax: Option<f64>,
}

fn positive(value: &str) -> Result<f64, String> {
    let number: f64 = value
        .parse()
        .map_err(|_| "Expected a positive number".to_string())?;
    if number.is_finite() && number > 0.0 && Duration::try_from_secs_f64(number).is_ok() {
        Ok(number)
    } else {
        Err("Expected a finite positive number representable as a duration".to_string())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        io::stdout().is_terminal() && io::stdin().is_terminal(),
        "Run pingolo in a Kitty terminal"
    );
    ensure!(
        !Duration::from_secs_f64(1.0 / f64::from(args.fps.get())).is_zero(),
        "FPS exceeds the timer's nanosecond resolution"
    );
    let mut terminal = Terminal::open(args.scale)?;
    let mut child = ping::spawn(args.address, args.interval, args.timeout)?;
    let result = run(&args, &mut terminal, &mut child).await;

    // Reap the ping process before restoring the terminal, including on errors.
    let cleanup = match child.try_wait()? {
        Some(_) => Ok(()),
        None => child.kill().await,
    };
    let restore = terminal.restore();
    result?;
    cleanup?;
    restore
}

async fn run(args: &Args, terminal: &mut Terminal, child: &mut Child) -> Result<()> {
    let stdout = child.stdout.take().context("Missing ping output pipe")?;
    let stderr = child.stderr.take().context("Missing ping error pipe")?;
    let mut output = BufReader::new(stdout).lines();
    let mut errors = BufReader::new(stderr).lines();
    let mut output_open = true;
    let mut errors_open = true;
    let mut error = String::new();
    let mut events = EventStream::new();
    let mut history = History::default();
    let interval = Duration::from_secs_f64(args.interval);
    let timeout = Duration::from_secs_f64(args.timeout);
    let window = Duration::from_secs_f64(args.window);
    let mut frames = time::interval(Duration::from_secs_f64(1.0 / f64::from(args.fps.get())));
    let mut fps_start = Instant::now();
    let mut frame_count = 0;
    let mut actual_fps = 0.0;

    loop {
        // System ping schedules probes independently; input waits do not spin.
        tokio::select! {
            line = output.next_line(), if output_open => {
                match line? {
                    Some(line) => {
                        if let Some(reply) = ping::parse(&line) {
                            history.record(reply, Instant::now(), interval);
                        }
                    }
                    None => output_open = false,
                }
            }
            line = errors.next_line(), if errors_open => {
                match line? {
                    Some(line) => error = line,
                    None => errors_open = false,
                }
            }
            status = child.wait() => {
                let status = status?;
                while let Some(line) = errors.next_line().await? {
                    error = line;
                }
                bail!("ping exited ({status}): {error}");
            }
            event = events.next() => {
                match event.context("Terminal input closed")?? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                            return Ok(());
                        }
                    }
                    Event::Resize(_, _) => terminal.resize()?,
                    _ => {},
                }
            }
            result = tokio::signal::ctrl_c() => {
                result?;
                return Ok(());
            }
            _ = frames.tick() => {
                let now = Instant::now();
                history.prune(now, window);
                frame_count += 1;
                let elapsed = now.duration_since(fps_start).as_secs_f64();
                if elapsed >= 1.0 {
                    actual_fps = f64::from(frame_count) / elapsed;
                    frame_count = 0;
                    fps_start = now;
                }
                let stats = history.stats(now, timeout);
                terminal.draw(&history, &stats, args, now, actual_fps)?;
                // Leave a full interval after each draw, including when output is slow.
                frames.reset();
            }
        }
    }
}
