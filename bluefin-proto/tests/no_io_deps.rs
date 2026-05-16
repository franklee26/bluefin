//! Sans-io guardrail: `bluefin-proto` must not depend on any I/O or
//! async-runtime crate. The whole point of the crate is to be the pure
//! protocol-state-machine layer; an accidental dependency on `tokio`,
//! `bluefin-io`, `mio`, `async-std`, or a raw socket type breaks the
//! sans-io boundary and lets I/O concerns leak across the seam we're
//! building in [`docs/SANS_IO_MIGRATION.md`](../../docs/SANS_IO_MIGRATION.md).
//!
//! This test reads the crate's `Cargo.toml` as text (no parser dep — we
//! want this guardrail to itself be dependency-free) and asserts that the
//! `[dependencies]` and `[dev-dependencies]` sections contain none of the
//! forbidden crate names. New deps are easy to add by accident; this test
//! is the cheapest possible "fail loud" signal in CI when that happens.

use std::fs;
use std::path::Path;

const FORBIDDEN_DEPS: &[&str] = &[
    "tokio",
    "tokio-util",
    "mio",
    "async-std",
    "smol",
    "futures",
    "bluefin-io",
];

#[test]
fn bluefin_proto_has_no_io_or_runtime_deps() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", manifest_path.display()));

    // Strip comments so a `# tokio = ...` note doesn't trip the check.
    let stripped: String = manifest
        .lines()
        .map(|l| match l.find('#') {
            Some(ix) => &l[..ix],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n");

    for dep in FORBIDDEN_DEPS {
        // Match either `tokio = ...` (line start) or `tokio.workspace = ...`
        // or the `[dependencies.tokio]` table form. We don't need a full
        // TOML parser to be confident; these are the only three shapes
        // cargo accepts for a top-level dep declaration.
        let needles = [
            format!("\n{dep} ="),
            format!("\n{dep}."),
            format!("\n[dependencies.{dep}]"),
            format!("\n[dev-dependencies.{dep}]"),
            format!("\n[build-dependencies.{dep}]"),
        ];
        for needle in &needles {
            assert!(
                !stripped.contains(needle.as_str()),
                "bluefin-proto must not depend on `{dep}` (sans-io guardrail). \
                 See docs/SANS_IO_MIGRATION.md §5 slice 0."
            );
        }
    }
}
