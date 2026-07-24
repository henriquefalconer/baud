// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The KVM-era guest-image contract (todo.md §4, specs/baud-packages.md §9): a workload is now a
// bootable guest image (kernel + rootfs + a tiny in-guest agent), not a single static ELF
// process. Two things the image's kernel config must get right before it can even boot
// deterministically under baud-multiverse:
//
//   1. It includes baud's tape-device driver/shim (specs/baud-tape-device.md §2: "the guest-side
//      driver is a tiny kernel shim shipped in the image ... see baud-packages") -- without it the
//      guest has no way to take entropy/clock/external-input from the tape at all.
//   2. It does NOT enable a real hardware timer baud-multiverse does not model -- RTC/HPET
//      (specs/baud-multiverse.md §3.3: "Delete HPET/PIT/PM-timer/RTC entirely"). baud-multiverse's
//      device bus never serves these ports/MMIO ranges (crates/baud-multiverse/src/console.rs's
//      `DeviceBus` only knows console + tape + open-bus fallback), so a guest kernel built with
//      them enabled would either hang waiting on a device that never answers, or -- worse for the
//      determinism guarantee -- read real host time through a path baud never intended to expose.
//
// `image_lint` checks a Linux kernel `.config` (the standard Kconfig-output format nix's kernel
// builder produces, and the same format a human editing `make menuconfig` produces) against this
// contract. This is `baud image lint` (todo.md §4's `image_lint_requires_tape_driver` test).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Kernel .config parsing
// ---------------------------------------------------------------------------

/// The state a Linux kernel `.config` assigns a Kconfig symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigState {
    /// `CONFIG_FOO=y` -- built in.
    Yes,
    /// `CONFIG_FOO=m` -- built as a loadable module.
    Module,
    /// `# CONFIG_FOO is not set` -- explicitly disabled (Kconfig's own way of saying "no").
    No,
}

/// A parsed Linux kernel `.config` file: every boolean/tristate Kconfig symbol recorded on the
/// way to producing the guest image's kernel. String- and int-valued symbols (`CONFIG_FOO="bar"`,
/// `CONFIG_FOO=42`) are outside this contract's concern and are not retained.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuestImageManifest {
    pub configs: BTreeMap<String, ConfigState>,
}

impl GuestImageManifest {
    /// Parse the standard `.config` text format Linux's Kconfig machinery emits:
    /// ```text
    /// CONFIG_FOO=y
    /// CONFIG_BAR=m
    /// # CONFIG_BAZ is not set
    /// # a free-form comment line, ignored
    /// ```
    /// Unrecognized lines (blank, non-`CONFIG_`, string/int-valued) are silently skipped --
    /// a real `.config` has thousands of such lines and only a handful matter to this contract.
    pub fn parse_kernel_config(text: &str) -> Self {
        let mut configs = BTreeMap::new();
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("# ") {
                if let Some(name) = rest.strip_suffix(" is not set") {
                    if name.starts_with("CONFIG_") {
                        configs.insert(name.to_string(), ConfigState::No);
                    }
                }
                // Any other comment line (section headers, etc.) carries no symbol; ignored.
                continue;
            }
            if let Some((name, value)) = line.split_once('=') {
                if !name.starts_with("CONFIG_") {
                    continue;
                }
                let state = match value {
                    "y" => ConfigState::Yes,
                    "m" => ConfigState::Module,
                    // String/int-valued symbols (quoted strings, hex addresses, etc.) are not
                    // part of this boolean/tristate contract.
                    _ => continue,
                };
                configs.insert(name.to_string(), state);
            }
        }
        GuestImageManifest { configs }
    }

    /// The recorded state of `symbol`, or `No` if the `.config` never mentioned it at all --
    /// Kconfig's own convention (an absent symbol and an explicit `# ... is not set` mean the
    /// same thing: the feature is off).
    pub fn state_of(&self, symbol: &str) -> ConfigState {
        self.configs
            .get(symbol)
            .copied()
            .unwrap_or(ConfigState::No)
    }

    /// True if `symbol` is built in (`y`) or built as a module (`m`) -- either way, the code is
    /// reachable at runtime, which is all this contract cares about.
    pub fn is_enabled(&self, symbol: &str) -> bool {
        matches!(
            self.state_of(symbol),
            ConfigState::Yes | ConfigState::Module
        )
    }
}

// ---------------------------------------------------------------------------
// The contract
// ---------------------------------------------------------------------------

/// The Kconfig symbol baud's tape-device guest-side kernel shim registers under
/// (specs/baud-tape-device.md §2). This is an out-of-tree baud driver, so a stock/vanilla kernel
/// config never has it -- its absence is exactly the failure mode this lint exists to catch
/// before a bad image gets booted and silently produces no observations at all.
pub const TAPE_DEVICE_CONFIG: &str = "CONFIG_BAUD_TAPE_DEVICE";

/// Real hardware timers baud-multiverse's run loop does not model and therefore never serves on
/// the device bus (specs/baud-multiverse.md §3.3: "Delete HPET/PIT/PM-timer/RTC entirely"). Each
/// entry is `(Kconfig symbol, human-readable name for the violation message)`.
pub const FORBIDDEN_REAL_TIMERS: &[(&str, &str)] = &[
    ("CONFIG_RTC_CLASS", "the real-time-clock (RTC) subsystem"),
    ("CONFIG_RTC_DRV_CMOS", "the CMOS RTC driver"),
    ("CONFIG_HPET_TIMER", "the HPET high-precision event timer"),
    ("CONFIG_HPET_MMAP", "HPET userspace mmap support"),
];

/// One thing wrong with a guest image's kernel config, with a reason a human (or a CI log) can
/// act on directly -- todo.md §4: "fails `baud image lint` with a specific reason."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintViolation {
    pub symbol: String,
    pub reason: String,
}

/// The result of linting a guest image's kernel config. Empty `violations` means the image
/// satisfies the contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintReport {
    pub violations: Vec<LintViolation>,
}

impl LintReport {
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Lint a parsed guest-image kernel config against the tape-device image contract (todo.md §4).
///
/// Two checks, each independent (both can fail at once, and both are reported -- a caller
/// shouldn't have to fix one violation, re-lint, and discover the second):
///   1. `TAPE_DEVICE_CONFIG` must be enabled (`y` or `m`).
///   2. None of `FORBIDDEN_REAL_TIMERS` may be enabled.
pub fn image_lint(manifest: &GuestImageManifest) -> LintReport {
    let mut violations = Vec::new();

    if !manifest.is_enabled(TAPE_DEVICE_CONFIG) {
        violations.push(LintViolation {
            symbol: TAPE_DEVICE_CONFIG.to_string(),
            reason: format!(
                "{TAPE_DEVICE_CONFIG} is not enabled: the guest kernel has no tape-device driver, \
                 so it cannot take entropy, clock, or external input from the tape at all \
                 (todo.md §4's image contract; specs/baud-tape-device.md §2's guest-side driver)"
            ),
        });
    }

    for (symbol, human_name) in FORBIDDEN_REAL_TIMERS {
        if manifest.is_enabled(symbol) {
            violations.push(LintViolation {
                symbol: symbol.to_string(),
                reason: format!(
                    "{symbol} is enabled: {human_name} is a real hardware timer baud-multiverse \
                     does not model or serve on its device bus (specs/baud-multiverse.md §3.3 \
                     deletes it entirely) -- a guest kernel with it enabled will hang waiting on \
                     a device that never answers, or read time baud never intended to expose"
                ),
            });
        }
    }

    LintReport { violations }
}

/// Convenience: parse a raw kernel `.config` and lint it in one call -- what the CLI/server route
/// actually does with the file/request body it receives.
pub fn lint_kernel_config(text: &str) -> LintReport {
    image_lint(&GuestImageManifest::parse_kernel_config(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(enabled: &[&str], disabled: &[&str]) -> String {
        let mut out = String::new();
        for sym in enabled {
            out.push_str(&format!("{sym}=y\n"));
        }
        for sym in disabled {
            out.push_str(&format!("# {sym} is not set\n"));
        }
        out
    }

    /// Spec test (todo.md §4, test matrix #14): an image without the tape-device driver fails
    /// `baud image lint` with a specific reason.
    #[test]
    fn image_lint_requires_tape_driver() {
        let text = config_with(&[], &["CONFIG_RTC_CLASS", "CONFIG_HPET_TIMER"]);
        let report = lint_kernel_config(&text);
        assert!(!report.ok(), "missing tape driver must fail lint");
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.symbol == TAPE_DEVICE_CONFIG),
            "violation must name the missing tape-device symbol: {:?}",
            report.violations
        );
    }

    /// Spec test (todo.md §4): an image with a real RTC enabled fails lint with a specific
    /// reason, even when the tape driver is present.
    #[test]
    fn image_lint_rejects_real_rtc() {
        let text = config_with(
            &[TAPE_DEVICE_CONFIG, "CONFIG_RTC_CLASS"],
            &["CONFIG_HPET_TIMER"],
        );
        let report = lint_kernel_config(&text);
        assert!(!report.ok(), "real RTC enabled must fail lint");
        assert!(report
            .violations
            .iter()
            .any(|v| v.symbol == "CONFIG_RTC_CLASS"));
        // Tape driver was present -- that specific violation must NOT also fire.
        assert!(!report
            .violations
            .iter()
            .any(|v| v.symbol == TAPE_DEVICE_CONFIG));
    }

    /// Spec test (todo.md §4): an image with HPET enabled fails lint with a specific reason.
    #[test]
    fn image_lint_rejects_real_hpet() {
        let text = config_with(
            &[TAPE_DEVICE_CONFIG, "CONFIG_HPET_TIMER"],
            &["CONFIG_RTC_CLASS"],
        );
        let report = lint_kernel_config(&text);
        assert!(!report.ok(), "real HPET enabled must fail lint");
        assert!(report
            .violations
            .iter()
            .any(|v| v.symbol == "CONFIG_HPET_TIMER"));
    }

    /// A well-formed image -- tape driver present, no forbidden real timer enabled -- passes.
    #[test]
    fn well_formed_image_passes_lint() {
        let text = config_with(
            &[TAPE_DEVICE_CONFIG],
            &[
                "CONFIG_RTC_CLASS",
                "CONFIG_RTC_DRV_CMOS",
                "CONFIG_HPET_TIMER",
                "CONFIG_HPET_MMAP",
            ],
        );
        let report = lint_kernel_config(&text);
        assert!(
            report.ok(),
            "well-formed image must pass lint, got: {:?}",
            report.violations
        );
    }

    /// A tape driver built as a module (`=m`) satisfies the contract just as well as built-in
    /// (`=y`) -- the code is reachable either way.
    #[test]
    fn tape_driver_as_module_satisfies_contract() {
        let text = format!("{TAPE_DEVICE_CONFIG}=m\n");
        let report = lint_kernel_config(&text);
        assert!(!report
            .violations
            .iter()
            .any(|v| v.symbol == TAPE_DEVICE_CONFIG));
    }

    /// An empty config (no lines at all) fails on the tape-driver check but reports no bogus
    /// real-timer violations -- absence of a symbol means "off," which is the correct state for
    /// the forbidden timers.
    #[test]
    fn empty_config_fails_only_on_missing_tape_driver() {
        let report = lint_kernel_config("");
        assert_eq!(report.violations.len(), 1);
        assert_eq!(report.violations[0].symbol, TAPE_DEVICE_CONFIG);
    }

    /// Both a missing tape driver and an enabled real timer are reported together in one lint
    /// pass -- a caller should not have to fix-and-relint twice to see every violation.
    #[test]
    fn both_violation_kinds_reported_together() {
        let text = config_with(&["CONFIG_RTC_CLASS"], &[]);
        let report = lint_kernel_config(&text);
        assert_eq!(report.violations.len(), 2, "{:?}", report.violations);
    }

    /// `.config` parsing handles the real Kconfig text format: `=y`/`=m` assignments, `# ... is
    /// not set` disables, string-valued symbols (ignored), non-CONFIG_ lines (ignored), blank
    /// lines, and a leading comment banner (as real kernel `.config` files always have one).
    #[test]
    fn parse_kernel_config_handles_standard_format() {
        let text = r#"
#
# Automatically generated file; DO NOT EDIT.
# Linux/x86_64 6.6.0 Kernel Configuration
#
CONFIG_64BIT=y
CONFIG_BAUD_TAPE_DEVICE=y
# CONFIG_RTC_CLASS is not set
CONFIG_HPET_MMAP=m
CONFIG_LOCALVERSION="-baud"
not_a_config_line_at_all
CONFIG_NR_CPUS=1
"#;
        let manifest = GuestImageManifest::parse_kernel_config(text);
        assert_eq!(manifest.state_of("CONFIG_64BIT"), ConfigState::Yes);
        assert_eq!(
            manifest.state_of(TAPE_DEVICE_CONFIG),
            ConfigState::Yes
        );
        assert_eq!(manifest.state_of("CONFIG_RTC_CLASS"), ConfigState::No);
        assert_eq!(manifest.state_of("CONFIG_HPET_MMAP"), ConfigState::Module);
        // String-valued symbol: not retained (not a boolean/tristate concern).
        assert!(!manifest.configs.contains_key("CONFIG_LOCALVERSION"));
        // Int-valued symbol: also not retained.
        assert!(!manifest.configs.contains_key("CONFIG_NR_CPUS"));
        // Never mentioned at all: defaults to No, same as an explicit "is not set".
        assert_eq!(manifest.state_of("CONFIG_HPET_TIMER"), ConfigState::No);
    }

    // -----------------------------------------------------------------------
    // Property test: any subset of forbidden real timers enabled is caught, one violation each,
    // regardless of which subset or what order the .config lists them in.
    // -----------------------------------------------------------------------
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn any_subset_of_forbidden_timers_enabled_is_fully_reported(
            mask in prop::collection::vec(any::<bool>(), FORBIDDEN_REAL_TIMERS.len())
        ) {
            let mut text = format!("{TAPE_DEVICE_CONFIG}=y\n");
            let mut expected_enabled: Vec<&str> = Vec::new();
            for (enabled, (symbol, _)) in mask.iter().zip(FORBIDDEN_REAL_TIMERS.iter()) {
                if *enabled {
                    text.push_str(&format!("{symbol}=y\n"));
                    expected_enabled.push(symbol);
                } else {
                    text.push_str(&format!("# {symbol} is not set\n"));
                }
            }
            let report = lint_kernel_config(&text);
            prop_assert_eq!(report.violations.len(), expected_enabled.len());
            for symbol in expected_enabled {
                prop_assert!(report.violations.iter().any(|v| v.symbol == symbol));
            }
        }
    }
}
