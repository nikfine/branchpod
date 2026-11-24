//! Open files do not block removal on Unix: nothing to report.

use std::path::Path;

use super::Holder;

pub(crate) fn holders(_: &Path) -> Vec<Holder> {
    Vec::new()
}
