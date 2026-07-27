// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The timed-exit determinism check's report/comparator layer (specs/baud-fingerprint.md). The
// capture primitives themselves (`run_to_events`/`translate_gva`/`capture_fingerprint`) already
// live in `baud-multiverse::linux::Multiverse` (todo.md §14 item 8) — this crate's job is only
// what that item's own "still open" note named as missing: shaping a raw
// `baud_multiverse::linux::TimedExitFingerprint` into the spec's `Fingerprint` (adding the
// banner-tail slice and the "did we actually reach the expected point" check), rendering the
// exact console report, and comparing two fingerprints field-by-field.
//
// One real deviation from the spec's illustrative code, kept because it reflects what
// `baud-multiverse` actually produces rather than a byte layout invented before capture existed:
// `mem_hash` here is the `"blake3:<hex>"`-prefixed `String` `Multiverse::ram_hash()` already
// returns everywhere else in this codebase (persisted columns, other reports), not a raw
// `[u8; 32]` requiring a second hex codec just for this one report. `render`/`compare` both treat
// it as an opaque, comparable string.
//
// A second deliberate generalization: `specs/baud-fingerprint.md` §5 hardcodes the Ubuntu login
// banner into `capture`. Its own prose says a non-distro guest "supplies its own banner (or an
// empty one)", so `capture` here takes the expected banner (and the console-tail length to slice
// it from) as parameters instead of a hardcoded Ubuntu constant — the Ubuntu H9 caller (not yet
// written; H9 (d)/(e) remain unstarted, todo.md §14 item 8) passes `UBUNTU_BANNER`, and today's
// non-distro test fixtures (which print no banner at all) pass `None` to skip the check.

/// The four-field timed-exit fingerprint plus its console banner (specs/baud-fingerprint.md §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// `"vm0"` / `"vm1"` — the only per-VM-varying token by design (§3); never compared by
    /// [`compare`].
    pub label: String,
    /// The last `K` bytes emitted to the serial console up to the stop (§4 step 5, §5).
    pub banner: Vec<u8>,
    /// Deterministic events = retired conditional branches = the requested `target_rcb`.
    pub events: u64,
    /// Guest-virtual RIP at the stop.
    pub rip: u64,
    /// Guest-physical address of `rip`; `None` if unmapped.
    pub gpa: Option<u64>,
    /// `"blake3:<hex>"` of guest RAM at the stop (see this module's doc for why this is a
    /// `String`, not `[u8; 32]`).
    pub mem_hash: String,
}

impl Fingerprint {
    /// Render the exact console report block (specs/baud-fingerprint.md §3). Only `<label>`
    /// differs between two matching VMs by design — see [`compare`].
    pub fn render(&self) -> String {
        let gpa = self.gpa.map(|g| format!("0x{g:016x}")).unwrap_or_else(|| "unmapped".into());
        format!(
            "{banner}\n{l} - timed exit:\n\
             deterministic events = {n}\n\
             guest RIP = 0x{rip:016x} (-> guest physical = {gpa})\n\
             guest memory hash = {hash}\n\
             {l}: done\n",
            banner = String::from_utf8_lossy(&self.banner),
            l = self.label,
            n = self.events,
            rip = self.rip,
            hash = self.mem_hash,
        )
    }
}

/// What capturing a fingerprint can fail with (specs/baud-fingerprint.md §5, §8's
/// `missing_login_fails_capture`).
#[derive(Debug, thiserror::Error)]
pub enum FpError {
    /// The underlying capture hit an unmodeled exit — propagated from
    /// [`baud_multiverse::linux::Multiverse::capture_fingerprint`], not raised by this crate.
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    DeterminismHole(#[from] baud_vcpu::DeterminismHole),
    /// The guest reached `target_rcb` without its console tail ever showing the expected banner
    /// — the run did not reach the state the caller meant to fingerprint, so `capture` refuses to
    /// report a fingerprint for the wrong point rather than silently doing so.
    #[error(
        "did not reach the expected banner by event {events}: expected the console tail to end \
         with {expected:?}, found {found:?}"
    )]
    NoBanner { events: u64, expected: Vec<u8>, found: Vec<u8> },
}

/// The first field two fingerprints disagree on (specs/baud-fingerprint.md §6). `label` is
/// deliberately never a possible `field` value — see [`compare`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub field: &'static str,
    pub a: String,
    pub b: String,
}

impl Divergence {
    fn new(field: &'static str, a: impl std::fmt::Display, b: impl std::fmt::Display) -> Self {
        Self { field, a: a.to_string(), b: b.to_string() }
    }
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "determinism VIOLATED at {}: {} != {}", self.field, self.a, self.b)
    }
}

/// Compare two fingerprints field-by-field, not the rendered text, so a formatting change can
/// never mask a real divergence and the first differing field is always named
/// (specs/baud-fingerprint.md §6). `label` is intentionally excluded — two VMs are expected to
/// carry different labels and still match on everything else.
pub fn compare(a: &Fingerprint, b: &Fingerprint) -> Result<(), Divergence> {
    if a.banner != b.banner {
        return Err(Divergence::new(
            "banner",
            String::from_utf8_lossy(&a.banner),
            String::from_utf8_lossy(&b.banner),
        ));
    }
    if a.events != b.events {
        return Err(Divergence::new("deterministic events", a.events, b.events));
    }
    if a.rip != b.rip {
        return Err(Divergence::new("guest RIP", format!("{:#018x}", a.rip), format!("{:#018x}", b.rip)));
    }
    if a.gpa != b.gpa {
        return Err(Divergence::new("guest physical", format!("{:?}", a.gpa), format!("{:?}", b.gpa)));
    }
    if a.mem_hash != b.mem_hash {
        return Err(Divergence::new("guest memory hash", &a.mem_hash, &b.mem_hash));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{FpError, Fingerprint};
    use baud_multiverse::linux::Multiverse;

    /// Stop `vm` at exactly `target_rcb` deterministic events and capture the four-field
    /// fingerprint plus the last `banner_tail_len` bytes of its console output
    /// (specs/baud-fingerprint.md §4). If `expected_banner` is `Some`, the captured tail must end
    /// with it or this returns [`FpError::NoBanner`] instead of a fingerprint for the wrong point
    /// (§5) — pass `None` for a guest (like today's non-distro test fixtures) that prints no
    /// recognizable banner at all.
    pub fn capture(
        vm: &mut Multiverse,
        label: &str,
        target_rcb: u64,
        banner_tail_len: usize,
        expected_banner: Option<&[u8]>,
    ) -> Result<Fingerprint, FpError> {
        let raw = vm.capture_fingerprint(target_rcb)?;
        let tail_start = raw.console_output.len().saturating_sub(banner_tail_len);
        let banner = raw.console_output[tail_start..].to_vec();
        if let Some(expected) = expected_banner {
            if !banner.ends_with(expected) {
                return Err(FpError::NoBanner {
                    events: raw.events,
                    expected: expected.to_vec(),
                    found: banner,
                });
            }
        }
        Ok(Fingerprint {
            label: label.to_string(),
            banner,
            events: raw.events,
            rip: raw.rip,
            gpa: raw.gpa,
            mem_hash: raw.mem_hash,
        })
    }

    /// Real-`/dev/kvm` proof that this crate's `capture`/[`compare`](super::compare) wiring
    /// carries the whole-machine determinism property `Multiverse::capture_fingerprint`'s own
    /// `timed_exit_fingerprint_is_stable` test already established at the primitive layer
    /// (`crates/baud-multiverse/src/linux/mod.rs`) — this is that same property proven through
    /// the report/comparator layer H9's real cross-VM check (`cross_vm_fingerprint_matches`,
    /// still unstarted, todo.md §14 item 8) will actually call. It boots two *independent*
    /// `Multiverse` instances sequentially in one process, standing in for H9's true two-separate-
    /// process/two-core orchestration (`specs/baud-fingerprint.md` §7) until that's wired — the
    /// determinism claim under test (same image + tape + N ⇒ same fingerprint) does not depend on
    /// which process observed each boot.
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::{Path, PathBuf};

        fn timer_guest_kernel_path() -> PathBuf {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../baud-multiverse/tests/fixtures/timer-guest/bzImage")
        }

        #[test]
        fn two_independent_boots_produce_matching_fingerprints() {
            let kernel = timer_guest_kernel_path();
            let cmdline = "console=ttyS0";
            const TARGET_RCB: u64 = 100_000;

            let mut vm0 = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("vm0 boot failed");
            let f0 = capture(&mut vm0, "vm0", TARGET_RCB, 64, None).expect("vm0 capture failed");

            let mut vm1 = Multiverse::boot(&kernel, cmdline, 0, 1, vec![], None).expect("vm1 boot failed");
            let f1 = capture(&mut vm1, "vm1", TARGET_RCB, 64, None).expect("vm1 capture failed");

            assert_ne!(f0.label, f1.label);
            crate::compare(&f0, &f1)
                .expect("two independent boots of the same (image, tape, N) must match");
        }

        /// specs/baud-fingerprint.md §8's `missing_login_fails_capture`, adapted to a fixture that
        /// prints no banner at all: asking `capture` to require a banner it can never see must
        /// fail the capture rather than silently returning a fingerprint for the wrong state.
        #[test]
        fn wrong_expected_banner_is_rejected() {
            let kernel = timer_guest_kernel_path();
            let mut vm =
                Multiverse::boot(&kernel, "console=ttyS0", 0, 1, vec![], None).expect("boot failed");
            let err = capture(&mut vm, "vm0", 100_000, 64, Some(b"a banner timer-guest never prints"))
                .expect_err("timer-guest's console never contains this banner");
            assert!(matches!(err, FpError::NoBanner { .. }));
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::capture;

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(label: &str) -> Fingerprint {
        Fingerprint {
            label: label.into(),
            banner: b"Ubuntu 18.04.1 LTS ubuntu ttyS0\n\nubuntu login: ".to_vec(),
            events: 4_812_337,
            rip: 0xffff_ffff_81ab_c123,
            gpa: Some(0x0000_0001_00ab_c123),
            mem_hash: "blake3:1111111111111111111111111111111111111111111111111111111111111111"
                .to_string(),
        }
    }

    /// specs/baud-fingerprint.md §8's `render_is_byte_exact`.
    #[test]
    fn render_is_byte_exact() {
        let f = fp("vm0");
        assert_eq!(
            f.render(),
            "Ubuntu 18.04.1 LTS ubuntu ttyS0\n\nubuntu login: \nvm0 - timed exit:\n\
             deterministic events = 4812337\n\
             guest RIP = 0xffffffff81abc123 (-> guest physical = 0x0000000100abc123)\n\
             guest memory hash = blake3:1111111111111111111111111111111111111111111111111111111111111111\n\
             vm0: done\n"
        );
    }

    /// specs/baud-fingerprint.md §8's `render_is_byte_exact`, unmapped-`gpa` arm.
    #[test]
    fn render_reports_unmapped_gpa() {
        let mut f = fp("vm0");
        f.gpa = None;
        assert!(f.render().contains("-> guest physical = unmapped)"));
    }

    /// specs/baud-fingerprint.md §8's `compare_reports_first_divergence`.
    #[test]
    fn compare_reports_first_divergence() {
        let a = fp("vm0");
        let mut b = fp("vm1");
        b.mem_hash = "blake3:2222222222222222222222222222222222222222222222222222222222222222"
            .to_string();
        let d = compare(&a, &b).unwrap_err();
        assert_eq!(d.field, "guest memory hash");
    }

    /// The comparator must report the *first* divergent field, not every field that differs.
    #[test]
    fn compare_names_the_earliest_field_when_several_differ() {
        let a = fp("vm0");
        let mut b = fp("vm1");
        b.rip += 1;
        b.mem_hash = "blake3:3333333333333333333333333333333333333333333333333333333333333333"
            .to_string();
        let d = compare(&a, &b).unwrap_err();
        assert_eq!(d.field, "guest RIP");
    }

    /// specs/baud-fingerprint.md §8's `label_difference_is_not_a_divergence`.
    #[test]
    fn label_difference_is_not_a_divergence() {
        assert!(compare(&fp("vm0"), &fp("vm1")).is_ok());
    }

    #[test]
    fn banner_divergence_is_reported_by_content_not_by_label() {
        let a = fp("vm0");
        let mut b = fp("vm1");
        b.banner = b"a different banner entirely".to_vec();
        let d = compare(&a, &b).unwrap_err();
        assert_eq!(d.field, "banner");
    }
}
