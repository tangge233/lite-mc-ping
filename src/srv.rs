//! SRV resolution for `_minecraft._tcp.<host>`, with RFC 2782 ranking.

use std::sync::LazyLock;

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use rand::{Rng, RngExt};

use crate::error::Error;

/// A parsed SRV record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SrvRecord {
    pub(crate) priority: u16,
    pub(crate) weight: u16,
    pub(crate) port: u16,
    pub(crate) target: String,
}

/// Process-wide default resolver, built lazily on first SRV lookup.
///
/// [`TokioResolver`] is `Clone + Send + Sync`; its name-server pool
/// (`Arc<PoolContext>`) and answer cache (`moka`, shared on clone) are held
/// behind reference-counted handles, so a single instance is designed for
/// reuse and every lookup shares the same DNS cache. A per-call resolver would
/// discard that cache and re-read the system config each time.
///
/// Returns `None` when the system resolver config cannot be read. The failure
/// is cached (not retried), and callers fall back to a direct connection.
pub(crate) fn shared_resolver() -> Option<&'static TokioResolver> {
    static RESOLVER: LazyLock<Option<TokioResolver>> = LazyLock::new(|| {
        TokioResolver::builder_tokio()
            .ok()
            .and_then(|builder| builder.build().ok())
    });
    RESOLVER.as_ref()
}

/// Query `_minecraft._tcp.<host>` and pick one target per RFC 2782.
///
/// Returns `Ok(None)` when there is no usable record or the lookup fails —
/// the caller then falls back to the direct address.
pub(crate) async fn resolve_srv(
    resolver: &TokioResolver,
    host: &str,
    rng: &mut impl Rng,
) -> Result<Option<SrvRecord>, Error> {
    // Query as an FQDN (trailing dot) so the resolver's search domains are
    // not appended to `_minecraft._tcp.<host>`.
    let query = if host.ends_with('.') {
        format!("_minecraft._tcp.{host}")
    } else {
        format!("_minecraft._tcp.{host}.")
    };
    let lookup = match resolver.srv_lookup(query.as_str()).await {
        Ok(lookup) => lookup,
        // Any lookup error (NXDOMAIN, timeout, ...) means "no SRV record";
        // the caller then falls back to the direct address.
        Err(_) => return Ok(None),
    };

    let records: Vec<SrvRecord> = lookup
        .answers()
        .iter()
        .filter_map(srv_from_record)
        .collect();

    Ok(pick_srv(records, rng))
}

fn srv_from_record(record: &hickory_resolver::proto::rr::Record) -> Option<SrvRecord> {
    let srv = match &record.data {
        RData::SRV(srv) => srv,
        _ => return None,
    };
    // RFC 2782: a root target (".") marks the service as not available at
    // this domain; skip such records.
    if srv.target.is_root() {
        return None;
    }
    Some(SrvRecord {
        priority: srv.priority,
        weight: srv.weight,
        port: srv.port,
        target: srv.target.to_string().trim_end_matches('.').to_string(),
    })
}

/// RFC 2782 target selection: lowest priority group first; within the group,
/// choose weighted-random by `weight` (uniform when all weights are 0).
fn pick_srv(mut records: Vec<SrvRecord>, rng: &mut impl Rng) -> Option<SrvRecord> {
    if records.is_empty() {
        return None;
    }
    records.sort_by_key(|r| r.priority);
    let lowest = records[0].priority;
    let pool: Vec<SrvRecord> = records
        .into_iter()
        .take_while(|r| r.priority == lowest)
        .collect();

    let total: u64 = pool.iter().map(|r| u64::from(r.weight)).sum();

    if total == 0 {
        // All weights zero → uniform choice.
        let idx = rng.random_range(0..pool.len());
        return pool.into_iter().nth(idx);
    }

    // Weighted: pick the record whose running sum crosses the random point.
    let mut cursor = rng.random_range(0..total);
    for record in pool {
        let weight = u64::from(record.weight);
        if weight > cursor {
            return Some(record);
        }
        cursor -= weight;
    }
    // Unreachable when total > 0; keeps the return type satisfied.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn seeded() -> StdRng {
        StdRng::seed_from_u64(0x5EED)
    }

    fn rec(priority: u16, weight: u16, port: u16, target: &str) -> SrvRecord {
        SrvRecord {
            priority,
            weight,
            port,
            target: target.to_string(),
        }
    }

    #[test]
    fn empty_records_yield_none() {
        assert_eq!(pick_srv(vec![], &mut seeded()), None);
    }

    #[test]
    fn single_record_wins() {
        let picked = pick_srv(vec![rec(10, 5, 123, "mc.example.com")], &mut seeded());
        assert_eq!(picked, Some(rec(10, 5, 123, "mc.example.com")));
    }

    #[test]
    fn lowest_priority_group_wins() {
        let pool = vec![
            rec(10, 0, 1, "a.example.com"),
            rec(5, 0, 2, "b.example.com"),
            rec(5, 0, 3, "c.example.com"),
        ];
        let picked = pick_srv(pool, &mut seeded()).unwrap();
        assert_eq!(picked.priority, 5);
    }

    #[test]
    fn uniform_choice_with_zero_weights() {
        let pool = vec![
            rec(1, 0, 1, "a.example.com"),
            rec(1, 0, 2, "b.example.com"),
            rec(1, 0, 3, "c.example.com"),
        ];
        let picked = pick_srv(pool, &mut seeded()).unwrap();
        assert_eq!(picked.priority, 1);
        assert!([1, 2, 3].contains(&picked.port));
    }

    #[test]
    fn weighted_choice_stays_in_lowest_priority_pool() {
        // Weight 1 : 3 — the pick must be one of the two lowest-priority
        // records, not fixed to a hand-computed rng sequence.
        let pool = vec![rec(1, 1, 1, "a.example.com"), rec(1, 3, 2, "b.example.com")];
        let picked = pick_srv(pool, &mut seeded()).unwrap();
        assert_eq!(picked.priority, 1);
        assert!([1, 2].contains(&picked.port));
    }

    #[test]
    fn root_target_skipped_by_collector() {
        use hickory_resolver::proto::rr::rdata::SRV;
        use hickory_resolver::proto::rr::{Name, Record};

        let rec = Record::from_rdata(
            Name::from_ascii("_minecraft._tcp.example.com").unwrap(),
            60,
            RData::SRV(SRV::new(1, 0, 25565, Name::root())),
        );
        // Root target (RFC 2782 "service not available") is skipped.
        assert!(srv_from_record(&rec).is_none());
    }

    #[test]
    fn shared_resolver_is_reused() {
        // May be None in environments without system DNS config; when
        // available, every call must return the same instance (shared cache).
        if let (Some(a), Some(b)) = (shared_resolver(), shared_resolver()) {
            assert!(std::ptr::eq(a, b));
        }
    }
}
