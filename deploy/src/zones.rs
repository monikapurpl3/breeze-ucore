//! Time zones by name, and each one's POSIX TZ rule: what μCore needs, since a
//! chip has no zone database. The table is the tz database's own: every zone
//! file ends with its current rule (see the header of `zones.tsv`).

const TABLE: &str = include_str!("zones.tsv");

/// `(name, rule)` for every zone, sorted by name.
pub fn all() -> Vec<(&'static str, &'static str)> {
    TABLE.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).filter_map(|l| l.split_once('\t')).collect()
}

pub fn rule(name: &str) -> Option<&'static str> {
    all().into_iter().find(|(n, _)| *n == name).map(|(_, r)| r)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_rule_is_one_ucore_reads() {
        let all = super::all();
        assert!(all.len() > 400);
        for (name, rule) in all {
            assert!(ucore::tz::Rule::parse(rule).is_some(), "{name}: {rule}");
        }
    }

    #[test]
    fn known_zones() {
        assert_eq!(super::rule("Europe/Zagreb"), Some("CET-1CEST,M3.5.0,M10.5.0/3"));
        assert_eq!(super::rule("Asia/Kathmandu"), Some("<+0545>-5:45"));
        assert_eq!(super::rule("Nowhere/Atlantis"), None);
    }
}
