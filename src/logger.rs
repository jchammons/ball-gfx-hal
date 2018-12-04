use crate::ui;
use colored::Colorize;
use imgui::{im_str, ImStr, Ui};
use lazy_static::lazy_static;
use log::{self, Level, LevelFilter, Log, Metadata};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::ops::Range;
use std::str;

// 512kb max.
const MAX_LOG_SIZE: usize = 1024 * 512;

lazy_static! {
    pub static ref LOGGER: Logger = Logger::default();
}

/// Sets this the global logger.
pub fn apply() -> Result<(), log::SetLoggerError> {
    eprintln!("{} {}", log::STATIC_MAX_LEVEL, log::max_level());
    log::set_max_level(LevelFilter::Info);
    log::set_logger(&*LOGGER)
}

/// `Log` implementation that outputs to stdout as well as an in-game
/// debug window.
///
/// Storing log messages for display in the debug window is done
/// through a pair of ring buffers. One contains `Record`s, which
/// store the log level as well as the span of text. The other
/// contains just the text for messages, all in linear memory. When
/// text would extend past the end of the second ring buffer, it wraps
/// back to the beginning and overwrites previous messages. The
/// `Records` corresponding to any overwritten messages are then
/// removed.
#[derive(Debug)]
pub struct Logger {
    internal: Mutex<LoggerInternal>,
}

#[derive(Debug)]
struct LoggerInternal {
    // Records are cheap, infrequent, and have a small fixed size, so just use VecDeque.
    records: VecDeque<Record>,
    // The actual text is unknown size though.
    text: Vec<u8>,
    // Current output position in the text buffer.
    head: usize,
}

#[derive(Debug)]
struct Record {
    level: Level,
    span: Range<usize>,
}

struct LogWriter<'a> {
    text: &'a mut [u8],
    start: usize,
    head: usize,
}

impl Default for Logger {
    fn default() -> Logger {
        Logger {
            internal: Mutex::new(LoggerInternal {
                records: VecDeque::new(),
                text: vec![0; MAX_LOG_SIZE],
                head: 0,
            }),
        }
    }
}

impl<'a> Write for LogWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        assert!(buf.len() <= MAX_LOG_SIZE);
        // Gone over the buffer end, time to wrap around.
        if self.head + buf.len() > MAX_LOG_SIZE {
            // Copy the earlier parts of the message over the end of the buffer.
            for idx in self.start..self.head {
                self.text[idx - self.start] = self.text[idx];
            }
            self.start = 0;
            self.head = 0;
        }

        let old_head = self.head;
        self.head += buf.len();
        self.text[old_head..self.head].copy_from_slice(buf);

        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let mut internal = self.internal.lock();
            let head = internal.head;

            // Store message in the text buffer
            let mut writer = LogWriter {
                text: &mut internal.text,
                head,
                start: head,
            };
            // Format the log into the buffer, including the null
            // terminator.
            write!(&mut writer, "{}\0", record.args()).unwrap();
            let span = writer.start..writer.head;
            let start = writer.start;
            internal.head = writer.head;

            // Clear out log messages intersecting the overwritten buffer space.
            while internal
                .records
                .front()
                .map(|record| span.contains(&record.span.start))
                .unwrap_or(false)
            {
                internal.records.pop_front().unwrap();
            }

            // Output to stdout, re-using the format results.
            let level = match record.level() {
                Level::Error => "[ERROR]".red(),
                Level::Warn => "[WARN ]".yellow(),
                Level::Info => "[INFO ]".cyan(),
                Level::Debug => "[DEBUG]".blue(),
                Level::Trace => "[TRACE]".blue(),
            };
            let msg = unsafe {
                // Garaunteed to be UTF8 as long as the null
                // terminator isn't included, hence the head - 1.
                str::from_utf8_unchecked(&internal.text[start..internal.head - 1])
            };
            println!("{} {}", level, msg);

            internal.records.push_back(Record {
                level: record.level(),
                span,
            });
        }
    }

    fn flush(&self) {}
}

impl Logger {
    /// Draws the logger-related UI into the debug window.
    pub fn ui<'a>(&self, ui: &Ui<'a>) {
        let mut filter = log::max_level();
        if ui::enum_combo(
            ui,
            im_str!("Log level"),
            &mut filter,
            &[
                im_str!("off"),
                im_str!("error"),
                im_str!("warn"),
                im_str!("info"),
                im_str!("debug"),
                im_str!("trace"),
            ],
            &[
                LevelFilter::Off,
                LevelFilter::Error,
                LevelFilter::Warn,
                LevelFilter::Info,
                LevelFilter::Debug,
                LevelFilter::Trace,
            ],
            4,
        ) {
            log::set_max_level(filter);
        }

        let internal = self.internal.lock();
        let filter = log::max_level();
        ui.child_frame(im_str!("Log"), (0.0, 0.0))
            .show_borders(true)
            .always_show_vertical_scroll_bar(true)
            .build(|| {
                for record in internal.records.iter() {
                    if record.level <= filter {
                        let (color, level) = match record.level {
                            Level::Error => ((0.75, 0.25, 0.25, 1.0), im_str!("[ERROR]")),
                            Level::Warn => ((0.75, 0.75, 0.25, 1.0), im_str!("[WARN ]")),
                            Level::Info => ((0.25, 0.25, 0.75, 1.0), im_str!("[INFO ]")),
                            Level::Debug => ((0.5, 0.5, 0.5, 1.0), im_str!("[DEBUG]")),
                            Level::Trace => ((0.25, 0.25, 0.25, 1.0), im_str!("[TRACE]")),
                        };
                        ui.text_colored(color, level);
                        ui.same_line(0.0);
                        let msg = unsafe {
                            ImStr::from_utf8_with_nul_unchecked(&internal.text[record.span.clone()])
                        };
                        ui.text_wrapped(&msg);
                    }
                }
            })
    }
}
