/// Standard result type for runtime functions that return typed
/// errors to Rune scripts. Lives in its own module so `Result<T>`
/// doesn't shadow `std::result::Result` inside `error.rs`, where
/// `#[rune::function]` macros expand unqualified `Result` references.
pub(crate) type Result<T> = std::result::Result<T, super::error::Error>;
