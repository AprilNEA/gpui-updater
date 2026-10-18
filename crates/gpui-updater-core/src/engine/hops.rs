//! Rebuilding an update from one or more deltas.

use std::path::{Path, PathBuf};

use super::UpdateEngine;
use super::config::Integrity;
use crate::cancel::CancelToken;
use crate::delta::{self, Route};
use crate::error::{Error, ErrorKind, Result};
use crate::http::{self, Fetch};
use crate::release::{DeltaAsset, Release};
use crate::source::UpdateSource;
use crate::staging::safe_file_name;

impl<S: UpdateSource> UpdateEngine<S> {
    /// Try to rebuild the update from deltas. `None` means "use the full
    /// download": there is no usable chain of deltas, or applying one failed
    /// (logged).
    pub(super) fn try_deltas(
        &self,
        release: &Release,
        dir: &Path,
        cancel: &CancelToken,
        progress: &mut impl FnMut(u64, Option<u64>),
    ) -> Option<PathBuf> {
        if !self.config.deltas || !cfg!(any(target_os = "macos", target_os = "linux")) {
            return None;
        }
        let from = &self.config.current_version;
        let chain = self.plan(release)?;
        let hops = chain.len();
        match self.apply_chain(&chain, dir, cancel, progress) {
            Ok(rebuilt) => {
                tracing::info!(%from, to = %release.version, hops, "rebuilt update from deltas");
                Some(rebuilt)
            }
            Err(e) if e.kind() == ErrorKind::Cancelled => {
                tracing::debug!(%from, to = %release.version, "delta update cancelled");
                None
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    %from,
                    to = %release.version,
                    hops,
                    "delta update failed; falling back to the full download"
                );
                None
            }
        }
    }

    /// The cheapest chain of deltas the policy can accept, if it beats the
    /// full download.
    fn plan<'r>(&self, release: &'r Release) -> Option<Vec<&'r DeltaAsset>> {
        let skips_all = self.config.verification.skips_all();
        // Skip deltas outright when the policy could not accept the release,
        // rather than spending downloads on them.
        if !skips_all && self.config.precheck(release).is_err() {
            return None;
        }
        let route = Route {
            from: &self.config.current_version,
            to: &release.version,
            max_hops: self.config.max_delta_hops,
            full_size: release.asset.size,
        };
        delta::cheapest(&release.deltas, &route, |d| {
            skips_all
                || self
                    .config
                    .precheck_integrity(Integrity::of_delta(d))
                    .is_ok()
        })
    }

    /// Download, verify, and apply each delta of `chain` in turn, starting
    /// from the installed files. Returns the directory holding the result.
    fn apply_chain(
        &self,
        chain: &[&DeltaAsset],
        dir: &Path,
        cancel: &CancelToken,
        progress: &mut impl FnMut(u64, Option<u64>),
    ) -> Result<PathBuf> {
        let installed = self.install_root()?;
        let name = installed
            .file_name()
            .ok_or_else(|| Error::Delta("install root has no file name".into()))?
            .to_os_string();
        let total = chain
            .iter()
            .all(|d| d.asset.size > 0)
            .then(|| chain.iter().map(|d| d.asset.size).sum::<u64>());

        let mut base = installed;
        let mut previous_out: Option<PathBuf> = None;
        let mut done = 0u64;
        for hop in chain {
            let out_dir = dir.join(format!("delta-{}", hop.to));
            let result = cancel.check().and_then(|()| {
                let paths = (base.as_path(), out_dir.join(&name));
                self.apply_hop(hop, (paths.0, &paths.1), dir, cancel, &mut |got, _| {
                    progress(done + got, total);
                })
            });
            // Each intermediate tree is only needed as the next hop's base.
            if let Some(previous) = previous_out.take() {
                let _ = std::fs::remove_dir_all(previous);
            }
            if let Err(e) = result {
                let _ = std::fs::remove_dir_all(&out_dir);
                return Err(e);
            }
            done += hop.asset.size;
            base = out_dir.join(&name);
            previous_out = Some(out_dir);
        }
        previous_out.ok_or_else(|| Error::Delta("empty delta chain".into()))
    }

    /// Download one delta into `dir`, verify it, and rebuild `out` from
    /// `base` with it.
    fn apply_hop(
        &self,
        hop: &DeltaAsset,
        (base, out): (&Path, &Path),
        dir: &Path,
        cancel: &CancelToken,
        progress: &mut impl FnMut(u64, Option<u64>),
    ) -> Result<()> {
        let file = dir.join(safe_file_name(&hop.asset.name, "update.delta"));
        let result = (|| {
            let fetch = Fetch::new(&hop.asset.url)
                .expect_len(hop.asset.size)
                .max_len(self.config.max_download_size)
                .cancel(cancel)
                .http(&self.config.http);
            http::download(&fetch, &file, &mut *progress)?;
            self.config
                .verify_file(&file, Integrity::of_delta(hop), &hop.to)?;
            // The header must describe exactly this hop, so a validly signed
            // delta for another version pair cannot be replayed.
            let info = delta::read_info(&file)?;
            if info.from != hop.from || info.to != hop.to {
                return Err(Error::Delta(format!(
                    "delta rebuilds {} -> {}, expected {} -> {}",
                    info.from, info.to, hop.from, hop.to
                )));
            }
            let parent = out.parent().unwrap_or(dir);
            let _ = std::fs::remove_dir_all(parent);
            std::fs::create_dir_all(parent)?;
            delta::apply(&file, base, out).map(drop)
        })();
        let _ = std::fs::remove_file(&file);
        result
    }
}
