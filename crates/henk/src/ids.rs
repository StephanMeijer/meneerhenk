//! Run ids: `r-YYYYMMDD-xxxxxxxx`.

use henk_domain::run::RunId;
use rand::Rng as _;
use time::OffsetDateTime;

/// A fresh run id. Sortable by day, unique enough by eight random hex digits.
pub fn new_run_id() -> RunId {
    let now = OffsetDateTime::now_utc();
    let random: u32 = rand::rng().random();
    let text = format!(
        "r-{:04}{:02}{:02}-{random:08x}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    RunId::parse(text).unwrap_or_else(|_| {
        RunId::parse("r-invalid").unwrap_or_else(|_| unreachable!("constant id is valid"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_the_expected_shape_and_differ() {
        let a = new_run_id();
        let b = new_run_id();
        assert!(a.as_str().starts_with("r-20"));
        assert_eq!(a.as_str().len(), "r-20261003-0123abcd".len());
        assert_ne!(a, b);
    }
}
