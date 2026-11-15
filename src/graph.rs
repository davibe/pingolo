use std::env;
use std::io::{self, BufWriter, Write};
use std::mem;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use flate2::{Compress, Compression, FlushCompress, Status};
use tiny_skia::{Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::Args;
use crate::ping::{History, Stats};

const LEFT: u16 = 12;
const TOP: u16 = 4;
const BACKGROUND: Color = Color::Rgb {
    r: 15,
    g: 23,
    b: 33,
};
const TEXT: Color = Color::Rgb {
    r: 214,
    g: 225,
    b: 237,
};
const MUTED: Color = Color::Rgb {
    r: 132,
    g: 153,
    b: 174,
};
const TEAL: Color = Color::Rgb {
    r: 85,
    g: 227,
    b: 202,
};

struct Size {
    columns: u16,
    rows: u16,
    width: u32,
    height: u32,
    cell_height: u32,
}

impl Size {
    fn read(tmux: bool, scale: f64) -> Result<Self> {
        let window = terminal::window_size()?;
        ensure!(
            window.columns >= 40 && window.rows >= 14,
            "Enlarge the terminal to at least 40 columns and 14 rows"
        );
        let (cell_width, cell_height) = if tmux {
            let output = Command::new("tmux")
                .args([
                    "display-message",
                    "-p",
                    "-t",
                    &env::var("TMUX_PANE")?,
                    "#{window_cell_width} #{window_cell_height}",
                ])
                .output()?;
            ensure!(output.status.success(), "Could not read the tmux cell size");
            let text = String::from_utf8(output.stdout)?;
            let mut parts = text.split_whitespace();
            (
                parts.next().context("Missing cell width")?.parse::<u32>()?,
                parts
                    .next()
                    .context("Missing cell height")?
                    .parse::<u32>()?,
            )
        } else {
            (
                u32::from(window.width / window.columns),
                u32::from(window.height / window.rows),
            )
        };
        ensure!(
            cell_width > 0 && cell_height > 0,
            "Terminal pixel size is unavailable; run in Kitty"
        );
        let columns = window.columns - LEFT - 2;
        let rows = window.rows - TOP - 4;
        let width = (f64::from(u32::from(columns) * cell_width) * scale).round() as u32;
        let height = (f64::from(u32::from(rows) * cell_height) * scale).round() as u32;
        let cell_height = (f64::from(cell_height) * scale).round() as u32;
        ensure!(
            width > 0 && height > 0 && cell_height > 0,
            "Resolution scale is too small for this terminal"
        );
        Ok(Self {
            columns,
            rows,
            width,
            height,
            cell_height,
        })
    }

    fn y(&self, fraction: f32) -> f32 {
        self.cell_height as f32 * 0.5 + fraction * (self.height - self.cell_height) as f32
    }
}

pub struct Terminal {
    output: BufWriter<io::Stdout>,
    tmux: bool,
    scale: f64,
    id: u32,
    active: bool,
    image_created: bool,
    size: Size,
    graph: Graph,
    compressor: Compress,
    // Reused output scratch keeps frame-sized allocations out of the render loop.
    compressed: Vec<u8>,
    encoded: String,
}

impl Terminal {
    pub fn open(scale: f64) -> Result<Self> {
        let tmux = env::var_os("TMUX").is_some();
        if tmux {
            let output = Command::new("tmux")
                .args(["show-options", "-gv", "allow-passthrough"])
                .output()?;
            ensure!(
                output.status.success() && String::from_utf8_lossy(&output.stdout).trim() != "off",
                "Enable Kitty images in tmux with: tmux set -g allow-passthrough on"
            );
        }
        let size = Size::read(tmux, scale)?;
        let graph = Graph::new(&size)?;
        let output = BufWriter::new(io::stdout());
        let compressor = Compress::new(Compression::fast(), true);
        let id = 0x400000 | (std::process::id() & 0x3fffff);
        terminal::enable_raw_mode()?;
        let mut terminal = Self {
            output,
            tmux,
            scale,
            id,
            active: true,
            image_created: false,
            size,
            graph,
            compressor,
            compressed: Vec::new(),
            encoded: String::new(),
        };
        execute!(
            terminal.output,
            EnterAlternateScreen,
            Hide,
            SetBackgroundColor(BACKGROUND),
            Clear(ClearType::All)
        )?;
        Ok(terminal)
    }

    pub fn resize(&mut self) -> Result<()> {
        let size = Size::read(self.tmux, self.scale)?;
        self.graph = Graph::new(&size)?;
        self.size = size;
        self.delete_image()?;
        self.image_created = false;
        execute!(self.output, Clear(ClearType::All))?;
        Ok(())
    }

    pub fn restore(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let image_result = self.delete_image();
        let output_result = execute!(self.output, ResetColor, Show, LeaveAlternateScreen);
        let mode_result = terminal::disable_raw_mode();
        image_result?;
        output_result?;
        mode_result?;
        Ok(())
    }

    fn delete_image(&mut self) -> Result<()> {
        graphics_start(&mut self.output, self.tmux)?;
        write!(self.output, "a=d,d=I,i={},q=2", self.id)?;
        graphics_end(&mut self.output, self.tmux)?;
        Ok(())
    }

    pub fn draw(
        &mut self,
        history: &History,
        stats: &Stats,
        args: &Args,
        now: Instant,
        fps: f64,
    ) -> Result<()> {
        let ymax = args.ymax.unwrap_or_else(|| nice_maximum(stats.max));
        self.graph
            .draw(&self.size, history, now, args.window, args.timeout, ymax)?;
        self.compressed.clear();
        self.compressed
            .reserve(self.graph.pixels.data().len() + self.graph.pixels.data().len() / 1000 + 128);
        self.compressor.reset();
        let status = self.compressor.compress_vec(
            self.graph.pixels.data(),
            &mut self.compressed,
            FlushCompress::Finish,
        )?;
        ensure!(
            status == Status::StreamEnd,
            "Could not compress the complete frame"
        );
        self.encoded.clear();
        STANDARD.encode_string(&self.compressed, &mut self.encoded);

        queue!(
            self.output,
            MoveTo(0, 0),
            SetForegroundColor(TEAL),
            Clear(ClearType::CurrentLine)
        )?;
        write!(self.output, "  pingolo  {}", args.address)?;
        queue!(
            self.output,
            MoveTo(0, 1),
            SetForegroundColor(MUTED),
            Clear(ClearType::CurrentLine)
        )?;
        write!(
            self.output,
            "  ICMP {:.1} Hz   render {:.0} fps   window {:.0}s",
            1.0 / args.interval,
            fps,
            args.window
        )?;
        queue!(
            self.output,
            MoveTo(0, 2),
            SetForegroundColor(TEXT),
            Clear(ClearType::CurrentLine)
        )?;
        match stats.last {
            Some(last) => write!(
                self.output,
                "  last {last:.2} ms   avg {:.2}   min {:.2}   max {:.2}",
                stats.average, stats.min, stats.max
            )?,
            None => write!(self.output, "  Waiting for replies...")?,
        }
        queue!(self.output, SetForegroundColor(MUTED))?;
        for tick in 0..=4 {
            let row = TOP + tick * (self.size.rows - 1) / 4;
            queue!(self.output, MoveTo(0, row))?;
            write!(
                self.output,
                "{:>8.2} ms ",
                ymax * (1.0 - f64::from(tick) / 4.0)
            )?;
        }
        let bottom = TOP + self.size.rows;
        queue!(
            self.output,
            MoveTo(0, bottom),
            Clear(ClearType::CurrentLine)
        )?;
        for tick in 0..=4 {
            let column = LEFT + tick * (self.size.columns - 5) / 4;
            queue!(self.output, MoveTo(column, bottom))?;
            if tick == 4 {
                write!(self.output, "now")?;
            } else {
                write!(
                    self.output,
                    "-{:.0}s",
                    args.window * (1.0 - f64::from(tick) / 4.0)
                )?;
            }
        }
        queue!(
            self.output,
            MoveTo(0, bottom + 2),
            Clear(ClearType::CurrentLine)
        )?;
        let total = stats.received + stats.lost;
        let loss = if total == 0 {
            0.0
        } else {
            100.0 * stats.lost as f64 / total as f64
        };
        write!(
            self.output,
            "  loss {loss:.1}% ({}/{total})   red: timeout   q / Esc: quit",
            stats.lost
        )?;

        queue!(self.output, MoveTo(LEFT, TOP))?;
        let chunks = self.encoded.as_bytes().chunks(4096);
        let count = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            graphics_start(&mut self.output, self.tmux)?;
            if self.image_created {
                // Kitty 0.48 needs both action and frame number on continuation chunks.
                write!(self.output, "a=f,r=1,")?;
            }
            if index == 0 {
                if self.image_created {
                    // Edit the root frame in place: one image, one frame, no accumulation.
                    write!(self.output, "X=1,")?;
                } else {
                    write!(
                        self.output,
                        "a=T,C=1,c={},r={},",
                        self.size.columns, self.size.rows
                    )?;
                    if self.tmux {
                        write!(self.output, "U=1,")?;
                    }
                }
                write!(
                    self.output,
                    "f=32,o=z,N=1,i={},s={},v={},q=2,",
                    self.id, self.size.width, self.size.height
                )?;
            }
            write!(self.output, "m={};", u8::from(index + 1 < count))?;
            self.output.write_all(chunk)?;
            graphics_end(&mut self.output, self.tmux)?;
        }
        if !self.image_created && self.tmux {
            self.place_tmux_image()?;
        }
        self.image_created = true;
        self.output.flush()?;
        Ok(())
    }

    fn place_tmux_image(&mut self) -> Result<()> {
        // Row indices follow the Kitty protocol's Unicode diacritic table.
        // https://github.com/kovidgoyal/kitty/blob/master/gen/rowcolumn-diacritics.txt
        let rows: Vec<char> = include_str!("diacritics.txt").trim().chars().collect();
        ensure!(
            usize::from(self.size.rows) <= rows.len(),
            "The graph exceeds Kitty's Unicode placeholder row limit"
        );
        queue!(
            self.output,
            SetForegroundColor(Color::Rgb {
                r: (self.id >> 16) as u8,
                g: (self.id >> 8) as u8,
                b: self.id as u8,
            })
        )?;
        for row in 0..self.size.rows {
            queue!(self.output, MoveTo(LEFT, TOP + row))?;
            write!(
                self.output,
                "\u{10eeee}{}{}",
                rows[usize::from(row)],
                rows[0]
            )?;
            for _ in 1..self.size.columns {
                write!(self.output, "\u{10eeee}")?;
            }
        }
        queue!(self.output, SetForegroundColor(TEXT))?;
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn graphics_start(output: &mut impl Write, tmux: bool) -> io::Result<()> {
    output.write_all(if tmux {
        b"\x1bPtmux;\x1b\x1b_G"
    } else {
        b"\x1b_G"
    })
}

fn graphics_end(output: &mut impl Write, tmux: bool) -> io::Result<()> {
    output.write_all(if tmux { b"\x1b\x1b\\\x1b\\" } else { b"\x1b\\" })
}

fn nice_maximum(value: f64) -> f64 {
    let target = (value * 1.12).max(1.0);
    let unit = 10.0_f64.powf(target.log10().floor());
    for factor in [1.0, 2.0, 5.0, 10.0] {
        if unit * factor >= target {
            return unit * factor;
        }
    }
    unreachable!()
}

struct Graph {
    pixels: Pixmap,
    background: Pixmap,
    // Reused path storage for the changing trace.
    path: PathBuilder,
}

impl Graph {
    fn new(size: &Size) -> Result<Self> {
        let mut background =
            Pixmap::new(size.width, size.height).context("Could not allocate the graph")?;
        background.fill(tiny_skia::Color::from_rgba8(15, 23, 33, 255));
        let mut paint = Paint::default();
        paint.set_color_rgba8(36, 51, 68, 255);
        for tick in 0..=4 {
            let y = size.y(tick as f32 / 4.0);
            let x = (size.width - 1) as f32 * tick as f32 / 4.0;
            background.fill_rect(
                Rect::from_xywh(0.0, y, size.width as f32, 1.0).unwrap(),
                &paint,
                Transform::identity(),
                None,
            );
            background.fill_rect(
                Rect::from_xywh(x, 0.0, 1.0, size.height as f32).unwrap(),
                &paint,
                Transform::identity(),
                None,
            );
        }
        let pixels = background.clone();
        Ok(Self {
            pixels,
            background,
            path: PathBuilder::new(),
        })
    }

    fn draw(
        &mut self,
        size: &Size,
        history: &History,
        now: Instant,
        window: f64,
        timeout: f64,
        ymax: f64,
    ) -> Result<()> {
        self.pixels
            .data_mut()
            .copy_from_slice(self.background.data());
        self.path.clear();
        let mut previous = None;
        let mut loss_paint = Paint::default();
        loss_paint.set_color_rgba8(255, 112, 124, 255);
        for sample in &history.samples {
            let age = now.saturating_duration_since(sample.sent).as_secs_f64();
            if age > window {
                continue;
            }
            let x = ((1.0 - age / window) * f64::from(size.width - 1)) as f32;
            match sample.milliseconds {
                Some(ms) => {
                    let y = size.y((1.0 - ms / ymax).clamp(0.0, 1.0) as f32);
                    if previous != Some(sample.sequence - 1) {
                        self.path.move_to(x, y);
                        self.path.line_to(x + 0.1, y);
                    } else {
                        self.path.line_to(x, y);
                    }
                    previous = Some(sample.sequence);
                }
                None => {
                    previous = None;
                    if age >= timeout {
                        self.pixels.fill_rect(
                            Rect::from_xywh(x, size.y(1.0) - 8.0, 2.0, 8.0).unwrap(),
                            &loss_paint,
                            Transform::identity(),
                            None,
                        );
                    }
                }
            }
        }
        if !self.path.is_empty() {
            let path = mem::take(&mut self.path)
                .finish()
                .context("Invalid graph path")?;
            let mut paint = Paint::default();
            paint.set_color_rgba8(85, 227, 202, 255);
            let stroke = Stroke {
                width: (size.cell_height as f32 / 14.0).max(1.5),
                ..Stroke::default()
            };
            self.pixels
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            self.path = path.clear();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ping::Reply;

    #[test]
    fn graph_frames_roundtrip_through_zlib() {
        let size = Size {
            columns: 100,
            rows: 30,
            width: 1000,
            height: 600,
            cell_height: 20,
        };
        let mut graph = Graph::new(&size).unwrap();
        let mut history = History::default();
        let now = Instant::now();
        for sequence in 0..200 {
            let age = Duration::from_millis((200 - sequence) as u64 * 100);
            let ms = if sequence % 5 == 0 {
                10.0 + f64::from(sequence % 100)
            } else {
                5.0
            };
            history.record(
                Reply {
                    sequence,
                    milliseconds: Some(ms),
                },
                now - age,
                Duration::from_millis(100),
            );
        }
        graph.draw(&size, &history, now, 20.0, 1.0, 120.0).unwrap();
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(graph.pixels.data()).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoded = Vec::new();
        let mut decoder = flate2::read::ZlibDecoder::new(compressed.as_slice());
        io::Read::read_to_end(&mut decoder, &mut decoded).unwrap();
        assert_eq!(decoded, graph.pixels.data());
        assert!(compressed.len() < decoded.len() / 10);
    }
}
