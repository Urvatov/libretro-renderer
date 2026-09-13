use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "libretro-renderer", about = "Offline RetroArch shader renderer for video files")]
pub struct Args {
    /// Input video file path
    #[arg(short, long)]
    pub input: PathBuf,

    /// Path to RetroArch shader preset (.slangp)
    #[arg(short, long)]
    pub shader: PathBuf,

    /// Output video file path
    #[arg(short, long)]
    pub output: PathBuf,

    /// Output width (if omitted, uses shader's native output or input width)
    #[arg(long)]
    pub width: Option<u32>,

    /// Output height (if omitted, uses shader's native output or input height)
    #[arg(long)]
    pub height: Option<u32>,

    /// Downscale input to this resolution before applying shader
    #[arg(long)]
    pub input_width: Option<u32>,

    /// Downscale input height before applying shader
    #[arg(long)]
    pub input_height: Option<u32>,

    /// Video encoder (default: libx264)
    #[arg(long, default_value = "libx264")]
    pub encoder: String,

    /// CRF quality value (lower = better quality, default: 18)
    #[arg(long, default_value_t = 18)]
    pub crf: u32,

    /// Encoder preset (default: slow)
    #[arg(long, default_value = "slow")]
    pub preset: String,

    /// Pixel format for encoding (default: yuv420p)
    #[arg(long, default_value = "yuv420p")]
    pub pixel_format: String,
}
