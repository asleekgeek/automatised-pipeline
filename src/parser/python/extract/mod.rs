// parser::python::extract — node-extraction functions, split from python.rs
// to keep every file under the 300-line limit. Pure move, no logic change:
// submodules re-export through the globs below so the functions still call
// one another (and the parent's Ctx / TS_* consts) unqualified.

mod g1;
mod g2;
mod g3;

pub(super) use g1::*;
use g2::*;
use g3::*;
