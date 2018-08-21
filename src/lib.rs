extern crate byteorder;
extern crate codespan;
extern crate codespan_reporting;
#[macro_use]
extern crate failure;
#[cfg(test)]
extern crate goldenfile;
#[macro_use]
extern crate im;
extern crate lalrpop_util;
#[macro_use]
extern crate moniker;
extern crate num_bigint;
extern crate num_traits;
extern crate pretty;
#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;
extern crate unicode_xid;

pub mod semantics;
pub mod syntax;

#[cfg(feature = "cli")]
extern crate linefeed;
#[cfg(feature = "cli")]
#[macro_use]
extern crate structopt;
#[cfg(feature = "cli")]
extern crate term_size;

#[cfg(feature = "cli")]
pub mod cli;

use codespan::FileMap;
use codespan_reporting::Diagnostic;

use syntax::core;

pub fn load_file(file: &FileMap) -> Result<core::Module, Vec<Diagnostic>> {
    use semantics::TcEnv;
    use syntax::translation::{Desugar, DesugarEnv};

    let (concrete_module, errors) = syntax::parse::module(&file);
    let mut diagnostics = errors
        .iter()
        .map(|err| err.to_diagnostic())
        .collect::<Vec<_>>();

    let raw_module = concrete_module.desugar(&DesugarEnv::new());
    semantics::check_module(&TcEnv::default(), &raw_module).map_err(|err| {
        diagnostics.push(err.to_diagnostic());
        diagnostics
    })
}
