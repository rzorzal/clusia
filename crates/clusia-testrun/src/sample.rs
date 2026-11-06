//! One sampler per measured run, at the same rate on both sides: the run's process group
//! through libproc in the local sandbox, the container's cgroup files through `docker exec` in a
//! container. It ends with its run and keeps nothing running.

use std::collections::BTreeMap;
use std::time::Duration;

use clusia_core::cockpit::{COMPARE_MAX_SECS, SAMPLE_EVERY_MS, Sample};
use tokio::time::Instant;

use crate::docker::Docker;
use crate::exec::{Exec, GroupUsage, StopFlag, Stream};

/// Where a run's memory and CPU are read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// The process group of a local sandbox run.
    Local { group: u32 },
    /// The cgroup files inside a run container.
    Container { docker: Docker, name: String },
}

/// What the container's sampler script starts each of its lines with.
pub const SAMPLE_TAG: &str = "clusia-sample";

/// The longest sampler line read, in bytes.
const LINE_MAX: usize = 200;

/// Prints one `clusia-sample <memory.current> <inactive_file> <usage_usec>` line per 250 ms from
/// the container's cgroup v2 files, and `-` for a value it can't read so the fields never shift.
/// Only shell builtins and `sleep`, whose failure ends the loop.
pub const SAMPLER_SCRIPT: &str = r#"while :; do m=-; i=-; u=-; { read -r m < /sys/fs/cgroup/memory.current; } 2>/dev/null; while read -r k v; do [ "$k" = inactive_file ] && i=$v; done 2>/dev/null < /sys/fs/cgroup/memory.stat; while read -r k v; do [ "$k" = usage_usec ] && u=$v; done 2>/dev/null < /sys/fs/cgroup/cpu.stat; echo "clusia-sample ${m:--} ${i:--} ${u:--}"; sleep 0.25 || exit 0; done"#;

/// Keeps a run's points to the sampler's own rate, so a flood of well-formed lines from the
/// run's own code cannot grow memory: a point less than half a tick after the last kept one is
/// dropped, and no more than the round's ticks (plus some slack) are kept.
#[derive(Debug, Clone)]
pub struct PointGate {
    last: Option<u32>,
    kept: u64,
    cap: u64,
}

impl PointGate {
    /// The gate of a round that may last `limit`.
    pub fn new(limit: Duration) -> Self {
        let limit_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
        Self {
            last: None,
            kept: 0,
            cap: (limit_ms / SAMPLE_EVERY_MS).saturating_add(16),
        }
    }

    /// Whether the point at `t_ms` is kept; a kept point counts against the cap.
    pub fn admit(&mut self, t_ms: u32) -> bool {
        if self.kept >= self.cap {
            return false;
        }
        if let Some(last) = self.last
            && u64::from(t_ms) < u64::from(last) + SAMPLE_EVERY_MS / 2
        {
            return false;
        }
        self.last = Some(t_ms);
        self.kept += 1;
        true
    }
}

/// `(memory.current, inactive_file, usage_usec)` of one sampler line, each `None` when it is not
/// a number. A line that is too long, untagged or without exactly three fields is `None`.
pub fn parse_sample_line(line: &str) -> Option<(Option<u64>, Option<u64>, Option<u64>)> {
    if line.len() > LINE_MAX {
        return None;
    }
    let rest = line.strip_prefix(SAMPLE_TAG)?.strip_prefix(' ')?;
    let mut fields = rest.split_ascii_whitespace();
    let (a, b, c) = (fields.next()?, fields.next()?, fields.next()?);
    if fields.next().is_some() {
        return None;
    }
    Some((digits_number(a), digits_number(b), digits_number(c)))
}

/// `text` as a number only when it is all ASCII digits, so a sign such as `+5` is refused.
pub(crate) fn digits_number(text: &str) -> Option<u64> {
    text.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| text.parse().ok())
        .flatten()
}

/// Turns cumulative CPU readings into thousandths of a core over the time between them.
#[derive(Debug, Clone, Default)]
pub struct CpuMeter {
    /// The CPU time of each pid at the last local reading.
    pids: BTreeMap<u32, u64>,
    /// The cgroup's `usage_usec` and the arrival time (ns) of the last reading.
    cgroup: Option<(u64, u64)>,
}

impl CpuMeter {
    /// `(rss_kb, cpu_milli)` of a local reading `tick_ns` after the last one. CPU counts each
    /// pid from its own last reading, a new pid from zero, so a process that ended does not
    /// make the rest look idle.
    pub fn local(
        &mut self,
        usage: Option<&GroupUsage>,
        tick_ns: u64,
    ) -> (Option<u64>, Option<u32>) {
        let Some(usage) = usage else {
            return (None, None);
        };
        let rss: u64 = usage
            .procs
            .iter()
            .map(|p| p.rss_bytes)
            .fold(0, u64::saturating_add);
        let spent: u128 = usage
            .procs
            .iter()
            .map(|p| {
                u128::from(
                    p.cpu_ns
                        .saturating_sub(self.pids.get(&p.pid).copied().unwrap_or(0)),
                )
            })
            .sum();
        self.pids = usage.procs.iter().map(|p| (p.pid, p.cpu_ns)).collect();
        let cpu = (tick_ns > 0)
            .then(|| u32::try_from(spent * 1000 / u128::from(tick_ns)).unwrap_or(u32::MAX));
        (Some(rss / 1024), cpu)
    }

    /// `(rss_kb, cpu_milli)` of a container reading that arrived `at_ns` after the run's start.
    /// Memory is what `docker stats` shows: in use less the file cache.
    pub fn cgroup(
        &mut self,
        parsed: (Option<u64>, Option<u64>, Option<u64>),
        at_ns: u64,
    ) -> (Option<u64>, Option<u32>) {
        let (current, inactive, usage) = parsed;
        let rss = current
            .zip(inactive)
            .map(|(current, inactive)| current.saturating_sub(inactive) / 1024);
        let Some(usage) = usage else {
            return (rss, None);
        };
        let previous = self.cgroup.replace((usage, at_ns));
        let cpu = previous.and_then(|(last_usage, last_at)| {
            (usage >= last_usage && at_ns > last_at).then(|| {
                let spent = u128::from(usage - last_usage) * 1_000_000;
                u32::try_from(spent / u128::from(at_ns - last_at)).unwrap_or(u32::MAX)
            })
        });
        (rss, cpu)
    }
}

/// The whole lines of a stream that arrives in pieces. A line past `LINE_MAX` bytes is dropped
/// up to its end.
#[derive(Default)]
struct Lines {
    pending: Vec<u8>,
    skipping: bool,
}

impl Lines {
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut whole = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                if !self.skipping {
                    whole.push(String::from_utf8_lossy(&self.pending).into_owned());
                }
                self.pending.clear();
                self.skipping = false;
            } else if !self.skipping {
                self.pending.push(byte);
                if self.pending.len() > LINE_MAX {
                    self.pending.clear();
                    self.skipping = true;
                }
            }
        }
        whole
    }
}

fn ms_since(began: Instant, now: Instant) -> u32 {
    u32::try_from((now - began).as_millis()).unwrap_or(u32::MAX)
}

fn ns_since(began: Instant, now: Instant) -> u64 {
    u64::try_from((now - began).as_nanos()).unwrap_or(u64::MAX)
}

/// Pushes a point every 250 ms until `stop` fires. It is a future the run joins, never a task,
/// so nothing reads or execs once it has returned. `t_ms` counts from the call.
pub async fn sample(
    exec: &dyn Exec,
    probe: Probe,
    stop: &StopFlag,
    push: &mut (dyn FnMut(Sample) + Send),
) {
    let began = Instant::now();
    let every = Duration::from_millis(SAMPLE_EVERY_MS);
    let mut meter = CpuMeter::default();
    match probe {
        Probe::Local { group } => {
            let mut last = began;
            loop {
                // A stop that is due wins over a tick that is due.
                tokio::select! {
                    biased;
                    _ = stop.notified() => return,
                    _ = tokio::time::sleep_until(last + every) => {}
                }
                let now = Instant::now();
                let tick_ns = ns_since(last, now);
                last = now;
                let usage = exec.group_usage(group);
                let (rss_kb, cpu_milli) = meter.local(usage.as_ref(), tick_ns);
                push(Sample {
                    t_ms: ms_since(began, now),
                    rss_kb,
                    cpu_milli,
                });
            }
        }
        Probe::Container { docker, name } => {
            let mut gate = PointGate::new(Duration::from_secs(COMPARE_MAX_SECS));
            loop {
                if stop.fired() {
                    return;
                }
                let cmd = docker.command(["exec", name.as_str(), "/bin/sh", "-c", SAMPLER_SCRIPT]);
                let mut lines = Lines::default();
                let mut chunk = |stream: Stream, bytes: &[u8]| {
                    if stream != Stream::Out {
                        return;
                    }
                    for line in lines.feed(bytes) {
                        let Some(parsed) = parse_sample_line(&line) else {
                            continue;
                        };
                        let now = Instant::now();
                        let t_ms = ms_since(began, now);
                        // Lines come from inside the container, which the run's own code controls.
                        if !gate.admit(t_ms) {
                            continue;
                        }
                        let (rss_kb, cpu_milli) = meter.cgroup(parsed, ns_since(began, now));
                        push(Sample {
                            t_ms,
                            rss_kb,
                            cpu_milli,
                        });
                    }
                };
                let limit = Duration::from_secs(COMPARE_MAX_SECS);
                // An exec that fails (the container is not there yet, or already gone) or ends is
                // tried again a tick later, until the run ends.
                let _ = exec.stream(&cmd, limit, stop, &mut chunk).await;
                tokio::select! {
                    biased;
                    _ = stop.notified() => return,
                    _ = tokio::time::sleep(every) => {}
                }
            }
        }
    }
}

/// `(peak rss_kb, average cpu_milli)` of the points inside `window` (all of them for `None`).
/// A metric with no measured point is `None`.
pub fn summarize(points: &[Sample], window: Option<(u64, u64)>) -> (Option<u64>, Option<u32>) {
    let inside = points
        .iter()
        .filter(|p| window.is_none_or(|(from, to)| (from..=to).contains(&u64::from(p.t_ms))));
    let (mut peak, mut sum, mut count) = (None, 0u64, 0u64);
    for point in inside {
        peak = peak.max(point.rss_kb);
        if let Some(cpu) = point.cpu_milli {
            sum += u64::from(cpu);
            count += 1;
        }
    }
    let average = (count > 0).then(|| u32::try_from(sum / count).unwrap_or(u32::MAX));
    (peak, average)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clusia_core::cockpit::Sample;

    use super::*;
    use crate::exec::{GroupUsage, ProcUsage, Stream};
    use crate::testkit::{FakeExec, FakeReply};

    fn docker() -> Docker {
        Docker {
            cli: "/opt/homebrew/bin/docker".into(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        }
    }

    fn point(t_ms: u32, rss_kb: Option<u64>, cpu_milli: Option<u32>) -> Sample {
        Sample {
            t_ms,
            rss_kb,
            cpu_milli,
        }
    }

    fn usage(procs: &[(u32, u64, u64)]) -> GroupUsage {
        GroupUsage {
            procs: procs
                .iter()
                .map(|&(pid, rss_bytes, cpu_ns)| ProcUsage {
                    pid,
                    rss_bytes,
                    cpu_ns,
                })
                .collect(),
        }
    }

    #[test]
    fn hostile_sample_lines_are_ignored() {
        assert_eq!(
            parse_sample_line("clusia-sample 10485760 2097152 1000000"),
            Some((Some(10_485_760), Some(2_097_152), Some(1_000_000)))
        );
        assert_eq!(
            parse_sample_line("clusia-sample - - -"),
            Some((None, None, None))
        );
        assert_eq!(
            parse_sample_line("clusia-sample 12x 5 -3"),
            Some((None, Some(5), None))
        );
        assert_eq!(
            parse_sample_line("clusia-sample 99999999999999999999999 1 1"),
            Some((None, Some(1), Some(1)))
        );
        for line in [
            "clusia-sample 1 2 3 4".to_string(),
            "clusia-sample 1 2".to_string(),
            "clusia-sample\t1 2 3".to_string(),
            "other 1 2 3".to_string(),
            format!("clusia-sample 1 2 {}", "3".repeat(200)),
            String::new(),
        ] {
            assert_eq!(parse_sample_line(&line), None, "{line}");
        }
    }

    #[test]
    fn the_cpu_meter_counts_each_pid_from_its_last_reading() {
        let tick = 250_000_000;
        let mut meter = CpuMeter::default();
        let first = usage(&[(10, 2_097_152, 100_000_000), (11, 1_048_576, 0)]);
        assert_eq!(meter.local(Some(&first), tick), (Some(3072), Some(400)));
        assert_eq!(meter.local(None, tick), (None, None));
        let second = usage(&[(10, 2_097_152, 150_000_000), (12, 1024, 25_000_000)]);
        assert_eq!(
            meter.local(Some(&second), tick),
            (Some(2049), Some(300)),
            "pid 12 counts from 0"
        );
        let back = usage(&[(10, 1024, 140_000_000)]);
        assert_eq!(
            meter.local(Some(&back), tick),
            (Some(1), Some(0)),
            "only positive deltas"
        );
        assert_eq!(meter.local(Some(&back), 0), (Some(1), None));

        let mut cgroup = CpuMeter::default();
        assert_eq!(
            cgroup.cgroup((Some(10_485_760), Some(2_097_152), Some(1_000_000)), 0),
            (Some(8192), None)
        );
        assert_eq!(
            cgroup.cgroup(
                (Some(10_485_760), Some(2_097_152), Some(1_125_000)),
                250_000_000
            ),
            (Some(8192), Some(500))
        );
        assert_eq!(
            cgroup.cgroup((None, Some(1), None), 500_000_000),
            (None, None)
        );
        assert_eq!(
            cgroup.cgroup((Some(4096), None, Some(1_000)), 600_000_000),
            (None, None),
            "usage went back"
        );
        assert_eq!(
            cgroup.cgroup((Some(1024), Some(4096), Some(251_000)), 850_000_000),
            (Some(0), Some(1000))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_sampler_ends_with_the_run() {
        let fake = FakeExec::new();
        fake.group(
            4242,
            vec![
                Some(usage(&[(4242, 1_048_576, 50_000_000)])),
                Some(usage(&[(4242, 2_097_152, 100_000_000)])),
                None,
            ],
        );
        let stop = StopFlag::new();
        let mut points = Vec::new();
        {
            let mut push = |p: Sample| points.push(p);
            let run = async {
                tokio::time::sleep(Duration::from_millis(1100)).await;
                stop.fire();
            };
            tokio::join!(
                sample(&fake, Probe::Local { group: 4242 }, &stop, &mut push),
                run
            );
        }
        assert_eq!(
            points,
            [
                point(250, Some(1024), Some(200)),
                point(500, Some(2048), Some(200)),
                point(750, None, None),
                point(1000, None, None),
            ]
        );
        assert_eq!(fake.group_calls(), 4);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(fake.group_calls(), 4, "no read after the run");

        let fake = FakeExec::new();
        fake.on(
            "docker",
            &["exec"],
            FakeReply::chunks(
                vec![(
                    Stream::Out,
                    "clusia-sample 10485760 2097152 1000000\n".into(),
                )],
                0,
            ),
        );
        let stop = StopFlag::new();
        let mut points = Vec::new();
        {
            let mut push = |p: Sample| points.push(p);
            let probe = Probe::Container {
                docker: docker(),
                name: "clusia-run-acme-widgets-d782c8-7-3".into(),
            };
            let run = async {
                tokio::time::sleep(Duration::from_millis(1100)).await;
                stop.fire();
            };
            tokio::join!(sample(&fake, probe, &stop, &mut push), run);
        }
        let calls = fake.calls();
        assert_eq!(calls.len(), 5, "one exec per tick while the run lasts");
        let args: Vec<String> = calls[0]
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "exec",
                "clusia-run-acme-widgets-d782c8-7-3",
                "/bin/sh",
                "-c",
                SAMPLER_SCRIPT
            ]
        );
        assert!(calls[0].clear_env);
        assert_eq!(
            points.iter().map(|p| p.t_ms).collect::<Vec<_>>(),
            [0, 250, 500, 750, 1000]
        );
        assert_eq!(points[0], point(0, Some(8192), None));
        assert_eq!(points[1], point(250, Some(8192), Some(0)));
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(fake.calls().len(), 5, "no exec after the run");
    }

    #[tokio::test(start_paused = true)]
    async fn a_container_sampler_waits_for_its_container_and_skips_garbage() {
        let fake = FakeExec::new();
        let garbage = format!(
            "{}\nclusia-sample 1 2 3 4\nnot a sample\nclusia-sample 10485760 2097152 1000000\n",
            "x".repeat(10_000)
        );
        fake.on_sequence(
            "docker",
            &["exec"],
            vec![
                FakeReply::fail(
                    1,
                    "Error response from daemon: No such container: clusia-run-x-7-1",
                ),
                FakeReply::fail(
                    1,
                    "Error response from daemon: No such container: clusia-run-x-7-1",
                ),
                FakeReply::chunks(
                    vec![
                        (Stream::Out, garbage),
                        (Stream::Err, "clusia-sample 1 1 1\n".into()),
                    ],
                    0,
                ),
            ],
        );
        let stop = StopFlag::new();
        let mut points = Vec::new();
        {
            let mut push = |p: Sample| points.push(p);
            let probe = Probe::Container {
                docker: docker(),
                name: "clusia-run-x-7-1".into(),
            };
            let run = async {
                tokio::time::sleep(Duration::from_millis(600)).await;
                stop.fire();
            };
            tokio::join!(sample(&fake, probe, &stop, &mut push), run);
        }
        assert_eq!(fake.calls().len(), 3);
        assert_eq!(
            points,
            [point(500, Some(8192), None)],
            "stdout's one valid line only"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_flood_of_sample_lines_keeps_the_sampler_rate() {
        let fake = FakeExec::new();
        let flood = "clusia-sample 10485760 2097152 1000000\n".repeat(1_000_000);
        fake.on(
            "docker",
            &["exec"],
            FakeReply::chunks(vec![(Stream::Out, flood)], 0),
        );
        let stop = StopFlag::new();
        let mut points = Vec::new();
        {
            let mut push = |p: Sample| points.push(p);
            let probe = Probe::Container {
                docker: docker(),
                name: "clusia-run-acme-widgets-d782c8-7-3".into(),
            };
            let run = async {
                tokio::time::sleep(Duration::from_millis(1100)).await;
                stop.fire();
            };
            tokio::join!(sample(&fake, probe, &stop, &mut push), run);
        }
        assert_eq!(
            points.iter().map(|p| p.t_ms).collect::<Vec<_>>(),
            [0, 250, 500, 750, 1000],
            "one point per tick, however many lines a tick prints"
        );
    }

    #[test]
    fn a_point_gate_keeps_half_a_tick_apart_and_a_round_of_ticks() {
        let mut gate = PointGate::new(Duration::from_secs(1));
        let kept: Vec<u32> = (0..1_000_000u32).filter(|&t| gate.admit(t)).collect();
        assert_eq!(
            kept.len(),
            1000 / 250 + 16,
            "capped at the round's ticks plus slack"
        );
        assert!(kept.windows(2).all(|w| w[1] - w[0] >= 125), "{kept:?}");
        assert_eq!(&kept[..3], [0, 125, 250]);

        let mut gate = PointGate::new(Duration::from_secs(600));
        assert!(gate.admit(500));
        assert!(!gate.admit(500));
        assert!(!gate.admit(624));
        assert!(gate.admit(625));
    }

    #[test]
    fn summarize_takes_the_window() {
        let points = [
            point(0, Some(100), Some(500)),
            point(250, Some(300), None),
            point(500, None, Some(1000)),
            point(750, Some(200), Some(0)),
        ];
        assert_eq!(summarize(&points, None), (Some(300), Some(500)));
        assert_eq!(
            summarize(&points, Some((250, 500))),
            (Some(300), Some(1000))
        );
        assert_eq!(summarize(&points, Some((800, 900))), (None, None));
        assert_eq!(summarize(&[], None), (None, None));
    }
}
