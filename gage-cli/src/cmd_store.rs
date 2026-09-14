use clap::Subcommand;
use gage_store::InitOutcome;

#[derive(Subcommand)]
pub enum StoreCommand {
    /// Create the Gage store
    ///
    /// Creates a bare Git repository at `$GAGE_HOME/store.git`. Running
    /// it on an existing store is harmless.
    Init,
}

pub fn run(command: StoreCommand) {
    match command {
        StoreCommand::Init => init(),
    }
}

fn init() {
    let outcome = match gage_store::init() {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("gage store init: {e}");
            std::process::exit(1);
        }
    };
    let verb = match outcome {
        InitOutcome::Created => "Initialized empty",
        InitOutcome::Reinitialized => "Reinitialized existing",
    };
    println!(
        "{verb} Gage store in {}/",
        gage_store::store_path().display()
    );
}
