mod app;
mod doc;
mod markdown;
mod message;
mod options;
mod outline;
mod session;
mod style;
mod syntax;

use std::error::Error;

pub use options::ViewOptions;

pub async fn run(session_id: &str, options: ViewOptions) -> Result<(), Box<dyn Error>> {
    let document = session::load(session_id).await?;
    let mut terminal = ratatui::init();
    let result = app::run(&mut terminal, &document, &options);
    ratatui::restore();
    result?;
    Ok(())
}
