//! Create a binary delta between two builds, for release CI.
//!
//! ```text
//! cargo run -p gpui-updater-core --features delta --release --example make_delta -- \
//!     <old> <new> <from-version> <to-version> <out.delta>
//! ```
//!
//! `<old>` and `<new>` are both `.app` bundle directories (macOS) or both
//! executables (Linux) — the unpacked contents of two releases, not the `.dmg`
//! or `.tar.gz` themselves. Publish the result next to the new release as
//! `<asset>.from-<from-version>.delta` (GitHub Releases), or list it under the
//! asset's `deltas` (static manifest), and sign it like any other artifact.
//!
//! Pass `--verify` as a sixth argument to rebuild `<new>` from `<old>` with the
//! fresh delta and confirm the result before publishing it.

use std::path::Path;
use std::time::Instant;

use gpui_updater_core::{Version, delta};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [old, new, from, to, out, rest @ ..] = args.as_slice() else {
        eprintln!(
            "usage: make_delta <old> <new> <from-version> <to-version> <out.delta> [--verify]"
        );
        std::process::exit(2);
    };
    let verify = rest.iter().any(|a| a == "--verify");
    let (old, new, out) = (Path::new(old), Path::new(new), Path::new(out));

    let started = Instant::now();
    let info = delta::create(old, new, &Version::parse(from)?, &Version::parse(to)?, out)?;
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
        let scratch =
            std::env::temp_dir().join(format!("make-delta-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let rebuilt = scratch.join(new.file_name().ok_or("new path has no file name")?);
        let result = delta::apply(out, old, &rebuilt).map(|_| ());
        let _ = std::fs::remove_dir_all(&scratch);
        result?;
        println!("verified: the delta rebuilds every file of the new build");
    }
    Ok(())
}
