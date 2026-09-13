mod cli;
mod render;
mod video;
mod vulkan;

use clap::Parser;
use cli::Args;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = Args::parse();
    render::run(&args)
}
