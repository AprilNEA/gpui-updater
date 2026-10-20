//! Create binary deltas between builds, for release CI.
//!
//! ```text
//! # Which earlier releases to build deltas from (newest first in, newest first out):
//! cargo run -p gpui-updater-core --features delta --release --example make_delta -- \
//!     bases [--direct 3] 0.8.12 0.8.11 0.8.10 ...
//!
//! # One delta:
//! cargo run -p gpui-updater-core --features delta --release --example make_delta -- \
//!     <old> <new> <from-version> <to-version> <out.delta> [--verify]
//! ```
//!
//! `<old>` and `<new>` are both `.app` bundle directories (macOS) or both
//! executables (Linux, or a bare `.exe` on Windows) — the unpacked contents of
//! two releases, not the `.dmg` or `.tar.gz` themselves. Publish each delta
//! next to the new release as
//! `<asset>.from-<from-version>.delta` (GitHub Releases), or list it under the
//! asset's `deltas` (static manifest), and sign it like any other artifact,
//! naming the version it produces in the trusted comment so it cannot be
//! replayed as another one: `minisign -S -m <out.delta> -t "version:<to-version>"`.
//!
//! `--verify` rebuilds `<new>` from `<old>` with the fresh delta and confirms
//! the result before it is published.

use std::path::Path;
use std::time::Instant;

use gpui_updater_core::{Version, delta};

const DEFAULT_DIRECT: usize = 3;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, rest @ ..] if cmd == "bases" => bases(rest),
        [old, new, from, to, out, rest @ ..] => {
            let verify = rest.iter().any(|a| a == "--verify");
            let versions = delta::VersionPair::new(Version::parse(from)?, Version::parse(to)?);
            make(
                (Path::new(old), Path::new(new)),
                &versions,
                Path::new(out),
                verify,
            )
        }
        _ => {
            eprintln!(
                "usage: make_delta <old> <new> <from-version> <to-version> <out.delta> [--verify]\n       make_delta bases [--direct N] <previous versions, newest first>..."
            );
            std::process::exit(2);
        }
    }
}

fn bases(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let (direct, versions) = match args {
        [flag, n, rest @ ..] if flag == "--direct" => (n.parse()?, rest),
        _ => (DEFAULT_DIRECT, args),
    };
    let previous = versions
        .iter()
        .map(|v| Version::parse(v.trim_start_matches('v')))
        .collect::<Result<Vec<_>, _>>()?;
    for version in delta::delta_bases(&previous, direct) {
        println!("{version}");
    }
    Ok(())
}

fn make(
    (old, new): (&Path, &Path),
    versions: &delta::VersionPair,
    out: &Path,
    verify: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let info = delta::create(old, new, versions, out)?;
    let size = std::fs::metadata(out)?.len();
    println!(
        "{} -> {}: {} files ({} copied, {} patched, {} added), {} bytes, {:.1}s",
        info.from,
        info.to,
        info.files,
        info.copied,
        info.patched,
        info.added,
        size,
        started.elapsed().as_secs_f64()
    );

    if verify {
        let started = Instant::now();
        let scratch =
            std::env::temp_dir().join(format!("make-delta-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let rebuilt = scratch.join(new.file_name().ok_or("new path has no file name")?);
        let result = delta::apply(out, old, &rebuilt).map(|_| ());
        let _ = std::fs::remove_dir_all(&scratch);
        result?;
        println!(
            "verified in {:.1}s: the delta rebuilds every file of the new build",
            started.elapsed().as_secs_f64()
        );
    }
    Ok(())
}
