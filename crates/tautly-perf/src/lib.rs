//! Shared helpers of the performance gates in `benches/perf/`: memory readings, timing
//! statistics, the job summary, and the synthetic repository generator.

use std::io::Write as _;
use std::time::Duration;

#[path = "../../../benches/perf/gen_repo.rs"]
pub mod gen_repo;

pub const MB: u64 = 1024 * 1024;

/// Resident set size of process `pid` now, in bytes (`ps -o rss=`, which both macOS and
/// Linux report in KiB).
pub fn rss_of(pid: u32) -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");
    let kib: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("ps -o rss= said {:?}", out));
    kib * 1024
}

/// Resident set size of this process now, in bytes.
pub fn rss() -> u64 {
    rss_of(std::process::id())
}

/// Peak resident set size of this process so far, in bytes.
pub fn peak_rss() -> u64 {
    // SAFETY: getrusage only writes the struct passed to it.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) }, 0);
    let max = u64::try_from(ru.ru_maxrss).unwrap_or(0);
    if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    }
}

/// The median of `samples` (the upper one of an even count).
pub fn median(mut samples: Vec<Duration>) -> Duration {
    assert!(!samples.is_empty());
    samples.sort();
    samples[samples.len() / 2]
}

/// Print a measurement and add it to the GitHub job summary when there is one.
pub fn report(line: &str) {
    println!("{line}");
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY")
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
    {
        let _ = writeln!(f, "- {line}");
    }
}

/// Milliseconds with two decimals, for reports.
pub fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_takes_the_middle_and_rss_reads() {
        let d = Duration::from_millis;
        assert_eq!(median(vec![d(5), d(1), d(3)]), d(3));
        assert_eq!(median(vec![d(4), d(1), d(3), d(2)]), d(3));
        assert!(rss() > MB);
        assert!(peak_rss() >= rss() / 2);
    }
}
