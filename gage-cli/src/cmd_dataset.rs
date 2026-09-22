use clap::Subcommand;
use gage_store::{DatasetStore, Store};

#[derive(Subcommand)]
pub enum DatasetCommand {
    /// Create an empty dataset
    New,
}

pub fn new() {
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage dataset new: {e}");
            std::process::exit(1);
        }
    };
    let id = match DatasetStore::from(&store).create() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage dataset new: {e}");
            std::process::exit(1);
        }
    };
    println!("Created dataset {id}");
}
