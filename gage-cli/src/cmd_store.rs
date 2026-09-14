use clap::Subcommand;

#[derive(Subcommand)]
pub enum StoreCommand {
    /// Create a Gage store
    Init,
}

pub fn run(command: StoreCommand) {
    match command {
        StoreCommand::Init => init(),
    }
}

fn init() {
    println!("Hello Git store");
}
