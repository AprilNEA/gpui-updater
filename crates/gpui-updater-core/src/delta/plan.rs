//! Which deltas to download, and which to publish.
//!
//! A delta only upgrades one version pair, so a user several releases behind
//! needs either a delta from their exact version or a chain of deltas. The
//! client side picks the chain with the fewest bytes ([`cheapest`]); the
//! publisher side ([`delta_bases`]) decides which old versions each release
//! ships deltas from, so that such a chain always exists and stays short.

use std::collections::HashMap;

use semver::Version;

use super::FORMAT;
use crate::release::DeltaAsset;

/// The upgrade a chain of deltas must perform, and its limits.
pub(crate) struct Route<'a> {
    /// The installed version.
    pub(crate) from: &'a Version,
    /// The version to end on.
    pub(crate) to: &'a Version,
    /// Longest chain allowed.
    pub(crate) max_hops: usize,
    /// Size of the full artifact in bytes, `0` when unknown. A chain whose
    /// sizes are all known must be smaller than this.
    pub(crate) full_size: u64,
}

#[derive(Clone)]
struct Best {
    cost: u64,
    path: Vec<usize>,
}

impl Best {
    fn better_than(&self, other: &Self) -> bool {
        (self.cost, self.path.len()) < (other.cost, other.path.len())
    }
}

/// The cheapest chain of deltas for `route`, or `None` when no chain within
/// the hop limit exists or the full download is smaller.
///
/// Only deltas in [`FORMAT`] that move strictly forward within the route and
/// pass `usable` are considered. A delta of unknown size is ranked as if it
/// were as large as the full artifact, so chains with known sizes win ties;
/// such a chain is still used, since its real cost cannot be compared.
pub(crate) fn cheapest<'a>(
    deltas: &'a [DeltaAsset],
    route: &Route<'_>,
    usable: impl Fn(&DeltaAsset) -> bool,
) -> Option<Vec<&'a DeltaAsset>> {
    let edges: Vec<usize> = (0..deltas.len())
        .filter(|&i| {
            let d = &deltas[i];
            d.format == FORMAT
                && route.from <= &d.from
                && d.from < d.to
                && &d.to <= route.to
                && usable(d)
        })
        .collect();
    let unknown_cost = route.full_size.max(1);
    let cost_of = |d: &DeltaAsset| {
        if d.asset.size == 0 {
            unknown_cost
        } else {
            d.asset.size
        }
    };

    // Bellman-Ford limited to `max_hops` rounds: `layer` holds the best chain
    // ending at each version using exactly the hops of the current round.
    let mut layer: HashMap<&Version, Best> = HashMap::from([(
        route.from,
        Best {
            cost: 0,
            path: Vec::new(),
        },
    )]);
    let mut best: Option<Best> = None;
    for _ in 0..route.max_hops {
        let mut next: HashMap<&Version, Best> = HashMap::new();
        for &i in &edges {
            let d = &deltas[i];
            let Some(prev) = layer.get(&d.from) else {
                continue;
            };
            let mut path = prev.path.clone();
            path.push(i);
            let candidate = Best {
                cost: prev.cost.saturating_add(cost_of(d)),
                path,
            };
            if next.get(&d.to).is_none_or(|b| candidate.better_than(b)) {
                next.insert(&d.to, candidate);
            }
        }
        if let Some(arrived) = next.get(route.to) {
            if best.as_ref().is_none_or(|b| arrived.better_than(b)) {
                best = Some(arrived.clone());
            }
        }
        if next.is_empty() {
            break;
        }
        layer = next;
    }

    let chain: Vec<&DeltaAsset> = best?.path.into_iter().map(|i| &deltas[i]).collect();
    let all_known = chain.iter().all(|d| d.asset.size > 0);
    let total: u64 = chain.iter().map(|d| d.asset.size).sum();
    if route.full_size > 0 && all_known && total >= route.full_size {
        return None;
    }
    Some(chain)
}

/// The earlier releases to build deltas from when publishing a new release.
///
/// `previous` lists the earlier releases newest first, without the one being
/// published. The result is the `direct` newest of them, plus the releases 2,
/// 4, 8, … steps back (a skip list). When every release is published this
/// way, a user `d` releases behind (up to `previous.len()`) can always reach
/// the newest release in at most ⌊log₂ d⌋ + 1 deltas, while each release ships
/// only about `direct + log₂(previous.len())` of them.
///
/// ```
/// use gpui_updater_core::{Version, delta::delta_bases};
///
/// let previous: Vec<Version> = (0..20).rev().map(|minor| Version::new(1, minor, 0)).collect();
/// let bases: Vec<String> = delta_bases(&previous, 3).iter().map(|v| v.to_string()).collect();
/// assert_eq!(bases, ["1.19.0", "1.18.0", "1.17.0", "1.16.0", "1.12.0", "1.4.0"]);
/// ```
pub fn delta_bases(previous: &[Version], direct: usize) -> Vec<Version> {
    let mut bases: Vec<Version> = previous.iter().take(direct).cloned().collect();
    let mut step = 1usize;
    while let Some(next) = step.checked_mul(2) {
        step = next;
        // `step` releases back is index `step - 1`.
        match previous.get(step - 1) {
            Some(version) if step > direct => bases.push(version.clone()),
            Some(_) => {}
            None => break,
        }
    }
    bases
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::{Route, cheapest, delta_bases};
    use crate::release::{Asset, DeltaAsset};
    use semver::Version;

    fn v(minor: u64) -> Version {
        Version::new(1, minor, 0)
    }

    fn delta(from: u64, to: u64, size: u64) -> DeltaAsset {
        DeltaAsset {
            from: v(from),
            to: v(to),
            format: super::FORMAT.to_string(),
            asset: Asset {
                name: format!("app.from-{from}.to-{to}.delta"),
                url: format!("https://example.test/{from}-{to}"),
                size,
            },
            sha256: None,
            checksums_url: None,
            signature: None,
            signature_url: None,
            ed25519_signature: None,
        }
    }

    fn route<'a>(from: &'a Version, to: &'a Version, full_size: u64) -> Route<'a> {
        Route {
            from,
            to,
            max_hops: 6,
            full_size,
        }
    }

    fn hops(chain: &[&DeltaAsset]) -> Vec<(u64, u64)> {
        chain.iter().map(|d| (d.from.minor, d.to.minor)).collect()
    }

    #[test]
    fn prefers_the_cheapest_chain() {
        let deltas = vec![
            delta(1, 4, 900),
            delta(1, 2, 100),
            delta(2, 4, 200),
            delta(2, 3, 50),
        ];
        let (from, to) = (v(1), v(4));
        let chain = cheapest(&deltas, &route(&from, &to, 1000), |_| true).unwrap();
        assert_eq!(hops(&chain), [(1, 2), (2, 4)]);
    }

    #[test]
    fn falls_back_when_the_full_download_is_smaller() {
        let deltas = vec![delta(1, 2, 600), delta(2, 3, 600)];
        let (from, to) = (v(1), v(3));
        assert!(cheapest(&deltas, &route(&from, &to, 1000), |_| true).is_none());
        assert!(cheapest(&deltas, &route(&from, &to, 0), |_| true).is_some());
    }

    #[test]
    fn respects_the_hop_limit_and_filters() {
        let deltas: Vec<_> = (1..8).map(|i| delta(i, i + 1, 10)).collect();
        let (from, to) = (v(1), v(8));
        let mut limited = route(&from, &to, 1000);
        limited.max_hops = 6;
        assert!(cheapest(&deltas, &limited, |_| true).is_none());
        limited.max_hops = 7;
        assert_eq!(cheapest(&deltas, &limited, |_| true).unwrap().len(), 7);
        assert!(cheapest(&deltas, &limited, |d| d.from != v(4)).is_none());
    }

    #[test]
    fn ignores_deltas_outside_the_route() {
        let mut other_format = delta(1, 2, 1);
        other_format.format = "zstd".into();
        let deltas = vec![other_format, delta(0, 2, 1), delta(1, 3, 1), delta(1, 2, 5)];
        let (from, to) = (v(1), v(2));
        let chain = cheapest(&deltas, &route(&from, &to, 1000), |_| true).unwrap();
        assert_eq!(hops(&chain), [(1, 2)]);
        assert_eq!(chain[0].asset.size, 5);
    }

    #[test]
    fn ranks_unknown_sizes_last_but_still_uses_them() {
        let deltas = vec![delta(1, 3, 0), delta(1, 2, 300), delta(2, 3, 300)];
        let (from, to) = (v(1), v(3));
        let chain = cheapest(&deltas, &route(&from, &to, 1000), |_| true).unwrap();
        assert_eq!(hops(&chain), [(1, 2), (2, 3)]);
        let chain = cheapest(&deltas[..1], &route(&from, &to, 1000), |_| true).unwrap();
        assert_eq!(hops(&chain), [(1, 3)]);
    }

    #[test]
    fn skip_list_keeps_every_gap_within_the_hop_bound() {
        const RELEASES: u64 = 130;
        for direct in [1, 3] {
            // Publish every release with `delta_bases`, then check every gap.
            let mut deltas = Vec::new();
            for n in 1..RELEASES {
                let previous: Vec<Version> = (0..n).rev().map(v).collect();
                for base in delta_bases(&previous, direct) {
                    deltas.push(delta(base.minor, n, 1));
                }
            }
            for gap in 1..RELEASES {
                let (from, to) = (v(0), v(gap));
                let mut r = route(&from, &to, 0);
                r.max_hops = usize::MAX;
                let chain = cheapest(&deltas, &r, |_| true).unwrap();
                let bound = gap.ilog2() as usize + 1;
                assert!(chain.len() <= bound, "gap {gap}: {} hops", chain.len());
            }
        }
    }

    #[test]
    fn delta_bases_handles_short_histories() {
        assert_eq!(delta_bases(&[], 3), Vec::<Version>::new());
        assert_eq!(delta_bases(&[v(5)], 3), [v(5)]);
        assert_eq!(delta_bases(&[v(5), v(4)], 0), [v(4)]);
    }
}
