//! Where PostgreSQL releases differ, as small named checks on the flavor.
//!
//! Each gate names the behavior, not the version, so callers read as what
//! they need ("does this server have partitioned tables?"). An unknown
//! version passes every gate, as [`Flavor::at_least`] assumes a recent one.

use quarry_core::Flavor;

/// Materialized views (relkind `m`) arrived in 9.3.
pub fn has_materialized_views(flavor: &Flavor) -> bool {
    flavor.at_least(9, 3)
}

/// Declarative partitioning arrived in 10: partitioned parents have relkind
/// `p`, and `pg_class.relispartition` marks the partitions under them. The
/// column does not exist before 10, so queries must not name it there.
pub fn has_partitioned_tables(flavor: &Flavor) -> bool {
    flavor.at_least(10, 0)
}

/// Before 12, `extra_float_digits = 0` printed float4/float8 rounded to 6
/// and 15 significant digits. From 12 the default (1) prints the shortest
/// text that reads back exactly, so values survive the text protocol.
pub fn prints_floats_exactly(flavor: &Flavor) -> bool {
    flavor.at_least(12, 0)
}

/// From 14, `pg_class.reltuples` is -1 for a table that was never vacuumed
/// or analyzed, so 0 means "analyzed and empty". Before 14 a fresh table
/// showed 0 as well, which could not be told apart from an empty one.
pub fn reltuples_marks_never_analyzed(flavor: &Flavor) -> bool {
    flavor.at_least(14, 0)
}

/// The planner's row estimate for a relation, `None` when the server has
/// none worth showing. `relpages` disambiguates the pre-14 zero: a table
/// with pages has been vacuumed or analyzed, so its 0 is a real count.
pub fn estimated_rows(flavor: &Flavor, reltuples: f64, relpages: i64) -> Option<u64> {
    let known = if reltuples_marks_never_analyzed(flavor) {
        reltuples >= 0.0
    } else {
        reltuples > 0.0 || relpages > 0
    };
    // The cast saturates, and reltuples is a float4 estimate anyway.
    known.then(|| reltuples.max(0.0).round() as u64)
}

/// The `relkind`s the schema browser lists, as a SQL list.
pub fn listed_relkinds(flavor: &Flavor) -> String {
    let mut kinds = vec!["'r'", "'v'", "'f'"];
    if has_materialized_views(flavor) {
        kinds.push("'m'");
    }
    if has_partitioned_tables(flavor) {
        kinds.push("'p'");
    }
    kinds.join(", ")
}

/// A condition on `pg_class` alias `c` that hides partitions, which the
/// browser shows through their partitioned parent instead.
pub fn hide_partitions(flavor: &Flavor) -> &'static str {
    if has_partitioned_tables(flavor) {
        "NOT c.relispartition"
    } else {
        "true"
    }
}

#[cfg(test)]
mod tests {
    use quarry_core::Dialect;

    use super::*;

    fn pg(version: &str) -> Flavor {
        Flavor::new(Dialect::Postgres, version)
    }

    #[test]
    fn gates_follow_the_release() {
        assert!(!has_materialized_views(&pg("9.2.24")));
        assert!(has_materialized_views(&pg("9.3")));
        assert!(!has_partitioned_tables(&pg("9.6.24")));
        assert!(has_partitioned_tables(&pg("10.23")));
        assert!(!prints_floats_exactly(&pg("11.22")));
        assert!(prints_floats_exactly(&pg("12.0")));
        assert!(!reltuples_marks_never_analyzed(&pg("13.16")));
        assert!(reltuples_marks_never_analyzed(&pg("14.0")));
        assert!(reltuples_marks_never_analyzed(&pg("17beta1")));
        // An unknown version is taken as recent.
        let unknown = Flavor::unversioned(Dialect::Postgres);
        assert!(has_partitioned_tables(&unknown));
        assert!(reltuples_marks_never_analyzed(&unknown));
    }

    #[test]
    fn reads_row_estimates_per_release() {
        let (pg13, pg17) = (pg("13.16"), pg("17.2"));
        // Never analyzed.
        assert_eq!(estimated_rows(&pg17, -1.0, 0), None);
        assert_eq!(estimated_rows(&pg13, 0.0, 0), None);
        // Analyzed and empty: only 14+ can say so.
        assert_eq!(estimated_rows(&pg17, 0.0, 0), Some(0));
        assert_eq!(estimated_rows(&pg13, 0.0, 0), None);
        // Emptied after it had pages.
        assert_eq!(estimated_rows(&pg13, 0.0, 3), Some(0));
        // Analyzed with rows.
        assert_eq!(estimated_rows(&pg13, 1234.4, 9), Some(1234));
        assert_eq!(estimated_rows(&pg17, 1234.6, 9), Some(1235));
    }

    #[test]
    fn lists_the_relkinds_each_release_has() {
        assert_eq!(listed_relkinds(&pg("9.2")), "'r', 'v', 'f'");
        assert_eq!(listed_relkinds(&pg("9.6")), "'r', 'v', 'f', 'm'");
        assert_eq!(listed_relkinds(&pg("13.1")), "'r', 'v', 'f', 'm', 'p'");
        assert_eq!(hide_partitions(&pg("9.6")), "true");
        assert_eq!(hide_partitions(&pg("10.0")), "NOT c.relispartition");
    }
}
