// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

//! Keep the daemon and all of its Tokio worker threads on four currently quiet CPUs.

use std::fs;
use std::io;
use std::thread;
use std::time::Duration;

// The server runs the real baud-multiverse KVM loops in-process. They therefore share this
// process affinity mask, and two cores leave room for the rest of the host while still allowing
// the Tokio reactor and one guest worker to run concurrently.
const CPU_LIMIT: usize = 2;

fn stats() -> io::Result<Vec<(usize, u64, u64)>> {
    let text = fs::read_to_string("/proc/stat")?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            let cpu = name.strip_prefix("cpu")?.parse().ok()?;
            let values: Vec<u64> = fields.filter_map(|v| v.parse().ok()).collect();
            let idle = values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
            let total = values.iter().copied().sum();
            Some((cpu, idle, total))
        })
        .collect())
}

/// Pin the current process to the two least-busy CPUs in its existing affinity mask.
///
/// The short sample avoids choosing CPUs that happened to be idle before startup but are busy
/// now. Failure is returned instead of silently running without the advertised cap.
pub fn pin_to_quiet_cpus() -> io::Result<Vec<usize>> {
    let mut allowed = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    let rc =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut allowed) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let allowed: Vec<usize> = (0..libc::CPU_SETSIZE as usize)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &allowed) })
        .collect();
    if allowed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "current affinity mask is empty",
        ));
    }

    let first = stats()?;
    thread::sleep(Duration::from_millis(100));
    let second = stats()?;
    let before: std::collections::HashMap<usize, (u64, u64)> = first
        .into_iter()
        .map(|(cpu, idle, total)| (cpu, (idle, total)))
        .collect();
    let mut ranked: Vec<(usize, u64)> = second
        .into_iter()
        .filter(|(cpu, _, _)| allowed.contains(cpu))
        .map(|(cpu, idle, total)| {
            let (old_idle, old_total) = before.get(&cpu).copied().unwrap_or((0, 0));
            let idle_delta = idle.saturating_sub(old_idle);
            let total_delta = total.saturating_sub(old_total).max(1);
            (cpu, idle_delta.saturating_mul(100_000) / total_delta)
        })
        .collect();
    ranked.sort_by(|(cpu_a, idle_a), (cpu_b, idle_b)| {
        idle_b.cmp(idle_a).then_with(|| cpu_a.cmp(cpu_b))
    });
    let selected: Vec<usize> = ranked
        .into_iter()
        .take(CPU_LIMIT.min(allowed.len()))
        .map(|(cpu, _)| cpu)
        .collect();
    if selected.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "no measurable CPU available",
        ));
    }

    let mut mask = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    for &cpu in &selected {
        unsafe { libc::CPU_SET(cpu, &mut mask) };
    }
    let rc = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mask) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    #[test]
    fn daemon_affinity_is_at_most_two_cpus() {
        let cpus = super::pin_to_quiet_cpus().expect("Linux affinity must be available");
        assert!(!cpus.is_empty());
        assert!(cpus.len() <= 2);
    }
}
