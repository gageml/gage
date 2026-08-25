//! Gage tests: test runs (prompt and scanner tests), their storage,
//! and report building. The command layer lives in `gage-cli`
//! (`cmd_test`) as `gage test`.

pub mod results;
pub mod run;
pub mod score;
pub mod storage;
pub mod suite;
pub mod tokens;
pub mod view;

mod scanner;
