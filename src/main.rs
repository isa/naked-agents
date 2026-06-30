mod cli;
mod format;
mod model;
mod search;
mod source;
mod tui;

fn main() -> anyhow::Result<()> {
    cli::run()
}
