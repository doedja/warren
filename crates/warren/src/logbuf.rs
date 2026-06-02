//! In-memory ring of recent hub log lines, surfaced on the admin dashboard.
//!
//! A `tracing` fmt layer writes formatted records here (in addition to stdout)
//! via [`RingWriter`], so the dashboard can show what the hub is doing
//! (enrollments, disconnects with reasons, errors) without shell access. Bounded,
//! so it never grows without limit; process-global because the tracing
//! subscriber is process-global.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Max lines retained. ~500 recent records is plenty for eyeballing on a page.
const CAP: usize = 500;

static RING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

fn ring() -> &'static Mutex<VecDeque<String>> {
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAP)))
}

/// Recent hub log lines, oldest first.
pub fn recent() -> Vec<String> {
    ring().lock().unwrap().iter().cloned().collect()
}

fn push(line: &str) {
    let line = line.trim_end();
    if line.is_empty() {
        return;
    }
    let mut r = ring().lock().unwrap();
    if r.len() >= CAP {
        r.pop_front();
    }
    r.push_back(line.to_string());
}

/// `MakeWriter` for a `tracing_subscriber` fmt layer: each event gets a fresh
/// buffer; the formatted record is pushed to the ring when that buffer drops.
pub struct RingWriter;

/// Buffers one formatted record, then appends it to the ring on drop. The fmt
/// layer makes one writer per event and writes the whole record to it.
pub struct RingBuf(Vec<u8>);

impl Write for RingBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for RingBuf {
    fn drop(&mut self) {
        if let Ok(s) = std::str::from_utf8(&self.0) {
            push(s);
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RingWriter {
    type Writer = RingBuf;
    fn make_writer(&'a self) -> Self::Writer {
        RingBuf(Vec::new())
    }
}
