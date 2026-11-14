use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pkg_sandbox::SandboxStream;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BuildOutputMode {
    #[default]
    Normal,
    Quiet,
    Verbose,
}

#[derive(Debug)]
pub struct BuildProgress {
    package: String,
    ordinal: usize,
    total: usize,
    mode: BuildOutputMode,
    started: Instant,
    estimate: Option<Duration>,
    native_percent: Mutex<Option<u8>>,
    terminal: Arc<Mutex<()>>,
}

impl BuildProgress {
    pub fn new(
        package: String,
        ordinal: usize,
        total: usize,
        mode: BuildOutputMode,
        estimate: Option<Duration>,
        terminal: Arc<Mutex<()>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            package,
            ordinal,
            total,
            mode,
            started: Instant::now(),
            estimate,
            native_percent: Mutex::new(None),
            terminal,
        })
    }

    pub fn start(&self) {
        self.status("building");
    }

    pub fn finish(&self, outcome: &str) {
        self.status(outcome);
    }

    fn status(&self, outcome: &str) {
        if self.mode == BuildOutputMode::Quiet {
            return;
        }
        let _guard = self.terminal.lock().unwrap_or_else(|e| e.into_inner());
        println!(
            "[{}/{}] {}: {outcome}",
            self.ordinal, self.total, self.package
        );
    }

    pub fn output_line(&self, _stream: SandboxStream, line: &str) {
        if let Some(percent) = native_progress(line) {
            let mut current = self
                .native_percent
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *current = Some(current.map_or(percent, |old| old.max(percent)));
        }
        if self.mode != BuildOutputMode::Verbose {
            return;
        }
        let prefix = self.prefix();
        let _guard = self.terminal.lock().unwrap_or_else(|e| e.into_inner());
        println!("{prefix} {line}");
    }

    fn prefix(&self) -> String {
        let base = format!("[{}/{}", self.ordinal, self.total);
        let native = *self
            .native_percent
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(percent) = native {
            return format!("{base} | {percent}%]");
        }
        if let Some(percent) = estimated_progress(self.started.elapsed(), self.estimate) {
            return format!("{base} | ~{percent}%]");
        }
        format!("{base}]")
    }
}

fn estimated_progress(elapsed: Duration, estimate: Option<Duration>) -> Option<u8> {
    let estimate = estimate.filter(|duration| !duration.is_zero())?;
    let percent = (elapsed.as_secs_f64() * 100.0 / estimate.as_secs_f64()).floor();
    Some(percent.clamp(0.0, 99.0) as u8)
}

fn native_progress(line: &str) -> Option<u8> {
    let line = line.trim_start();
    let body = line.strip_prefix('[')?;
    let end = body.find(']')?;
    let marker = body[..end].trim();
    if let Some(percent) = marker.strip_suffix('%') {
        return percent.trim().parse::<u8>().ok().filter(|p| *p <= 100);
    }
    let (done, total) = marker.split_once('/')?;
    let done = done.trim().parse::<u64>().ok()?;
    let total = total.trim().parse::<u64>().ok()?;
    if total == 0 || done > total {
        return None;
    }
    Some(((done.saturating_mul(100) / total).min(100)) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_native_progress_only_when_it_has_a_total() {
        assert_eq!(native_progress("[12/40] CC file.o"), Some(30));
        assert_eq!(native_progress("[ 53%] Building CXX object x"), Some(53));
        assert_eq!(native_progress("[100%] Built target hello"), Some(100));
        assert_eq!(native_progress("cc -c file.c"), None);
        assert_eq!(native_progress("[12/0] bad"), None);
        assert_eq!(native_progress("[13/12] bad"), None);
        assert_eq!(native_progress("[101%] bad"), None);
    }

    #[test]
    fn estimates_are_marked_and_never_reach_completion_early() {
        let estimate = Some(Duration::from_secs(100));
        assert_eq!(
            estimated_progress(Duration::from_secs(53), estimate),
            Some(53)
        );
        assert_eq!(
            estimated_progress(Duration::from_secs(200), estimate),
            Some(99)
        );
        assert_eq!(estimated_progress(Duration::from_secs(1), None), None);
        assert_eq!(
            estimated_progress(Duration::from_secs(1), Some(Duration::ZERO)),
            None
        );
    }

    #[test]
    fn formats_exact_estimated_and_unknown_prefixes() {
        let terminal = Arc::new(Mutex::new(()));
        let progress = BuildProgress::new(
            "hello".into(),
            1,
            28,
            BuildOutputMode::Verbose,
            None,
            terminal.clone(),
        );
        assert_eq!(progress.prefix(), "[1/28]");
        progress.output_line(SandboxStream::Stdout, "[1/4] CC hello.c");
        assert_eq!(progress.prefix(), "[1/28 | 25%]");

        let mut estimated = BuildProgress::new(
            "hello".into(),
            2,
            28,
            BuildOutputMode::Verbose,
            Some(Duration::from_secs(100)),
            terminal,
        );
        Arc::get_mut(&mut estimated).unwrap().started = Instant::now() - Duration::from_secs(53);
        assert_eq!(estimated.prefix(), "[2/28 | ~53%]");
        assert_eq!(BuildOutputMode::default(), BuildOutputMode::Normal);
    }
}
