use std::time::{Duration, Instant};

/// Linux counts a process's cpu time in `USER_HZ`, which is 100 everywhere.
///
/// It is fixed at 100 in the kernel ABI for `/proc` regardless of the tick rate
/// the kernel was built with -- that is the point of `USER_HZ` being a separate
/// number from `HZ` -- so `getconf CLK_TCK` says 100 on every distribution
/// worth naming. Reading it properly means `sysconf`, and `sysconf` means
/// linking libc, and a dependency for a constant that has not moved since 2.4
/// is not worth it.
const USER_HZ: f64 = 100.0;

/// What `/proc/self/statm` counts resident memory in.
///
/// The same trade as `USER_HZ`: the honest answer is `sysconf(_SC_PAGESIZE)`,
/// and the practical one is that every x86-64 and every aarch64 kernel this
/// runs on pages at four kilobytes. A machine with 16K pages under-reports its
/// bots by a factor of four here, which is worth knowing and not worth a
/// dependency -- the number beside it, the server's own RSS, comes from
/// somewhere else anyway.
const PAGE_BYTES: u64 = 4_096;

/// What this process has spent, as of one moment.
///
/// The bots' own cost, not the server's. It is in the output for one reason:
/// a load bench that has run out of cpu measures itself, and a ceiling found
/// while the generator is pinned is a fact about the generator. Whoever reads
/// the run needs to be able to tell those apart without being told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub cpu: Duration,
    pub rss_bytes: u64,
    pub at: Instant,
}

/// What was spent between two [`Usage`] readings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spend {
    pub cpu: Duration,
    pub wall: Duration,
    /// Cpu time as a share of every core on this machine, in percent.
    ///
    /// Against all cores rather than one, so that 100 means "there is nothing
    /// left on this machine" on a laptop and on a build box alike. A number
    /// against a single core would read as 780% on the machine where it
    /// mattered and would have to be divided by a core count the reader has to
    /// go and find.
    pub cpu_percent: f64,
    pub rss_bytes: u64,
}

impl Usage {
    /// `None` anywhere without `/proc`, which is every platform but Linux.
    ///
    /// Deliberately not papered over with a crate that knows how to ask macOS:
    /// the run that decides a node's capacity happens on Linux, and a
    /// dependency carried by everything so that a developer's laptop can print
    /// one more number is a bad trade. Off Linux the output says so.
    pub fn read() -> Option<Self> {
        let cpu = cpu_time(&std::fs::read_to_string("/proc/self/stat").ok()?)?;
        let rss_bytes = resident_bytes(&std::fs::read_to_string("/proc/self/statm").ok()?)?;

        Some(Self {
            cpu,
            rss_bytes,
            at: Instant::now(),
        })
    }

    pub fn since(&self, earlier: &Usage) -> Spend {
        let cpu = self.cpu.saturating_sub(earlier.cpu);
        let wall = self.at.saturating_duration_since(earlier.at);
        let cores = std::thread::available_parallelism()
            .map(|cores| cores.get() as f64)
            .unwrap_or(1.0);

        let seconds = wall.as_secs_f64();
        let cpu_percent = if seconds > 0.0 {
            cpu.as_secs_f64() / seconds / cores * 100.0
        } else {
            0.0
        };

        Spend {
            cpu,
            wall,
            cpu_percent,
            rss_bytes: self.rss_bytes,
        }
    }
}

/// utime + stime out of `/proc/self/stat`.
///
/// Parsed from the last `)` rather than by splitting the whole line, because
/// the second field is the executable name in parentheses and an executable is
/// allowed to be called `mediacore load (old))`. Every field after it is a
/// number, so from there the count is reliable: state is field 3, and utime and
/// stime are fields 14 and 15.
fn cpu_time(stat: &str) -> Option<Duration> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after_name.split_whitespace().collect();

    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;

    Some(Duration::from_secs_f64((utime + stime) as f64 / USER_HZ))
}

/// The second field of `/proc/self/statm`, in pages.
fn resident_bytes(statm: &str) -> Option<u64> {
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * PAGE_BYTES)
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
