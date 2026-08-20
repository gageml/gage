//! Gage evals: test runs (prompt and scanner tests), their storage,
//! and report building. The command layer lives in `gage-cli`
//! (`cmd_eval`) as `gage eval`.

pub mod eval;
pub mod results;
pub mod run;
pub mod scan;
pub mod score;
pub mod storage;
pub mod tokens;
pub mod view;

mod scanner;
