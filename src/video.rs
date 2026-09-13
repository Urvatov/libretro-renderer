use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
    pub frame_rate_str: String,
    pub total_frames: u64,
    #[allow(dead_code)]
    pub duration_secs: f64,
}

pub fn probe_video(path: &Path) -> Result<VideoInfo> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "quiet",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
            "-select_streams", "v:0",
            &path.to_string_lossy(),
        ])
        .output()
        .context("Failed to run ffprobe. Is ffmpeg/ffprobe in PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffprobe failed: {}", stderr);
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("Failed to parse ffprobe JSON output")?;

    let stream = json["streams"]
        .as_array()
        .and_then(|s| s.first())
        .ok_or_else(|| anyhow::anyhow!("No video stream found"))?;

    let width = stream["width"].as_u64().unwrap_or(0) as u32;
    let height = stream["height"].as_u64().unwrap_or(0) as u32;

    let r_frame_rate = stream["r_frame_rate"]
        .as_str()
        .unwrap_or("30/1");
    let parts: Vec<&str> = r_frame_rate.split('/').collect();
    let fps: f64 = if parts.len() == 2 {
        let num: f64 = parts[0].parse().unwrap_or(30.0);
        let den: f64 = parts[1].parse().unwrap_or(1.0);
        if den > 0.0 { num / den } else { 30.0 }
    } else {
        r_frame_rate.parse().unwrap_or(30.0)
    };

    let duration_secs = json["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    let total_frames = stream["nb_frames"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or((duration_secs * fps) as u64);

    Ok(VideoInfo {
        width,
        height,
        frame_rate: fps,
        frame_rate_str: r_frame_rate.to_string(),
        total_frames,
        duration_secs,
    })
}

pub struct VideoDecoder {
    child: Child,
    frame_size: usize,
    frame_index: u64,
    #[allow(dead_code)]
    fps: f64,
}

impl VideoDecoder {
    pub fn open(path: &Path, target_width: u32, target_height: u32, fps: f64) -> Result<Self> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error"])
            .args(["-i", &path.to_string_lossy()])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba"]);

        if target_width > 0 && target_height > 0 {
            cmd.args(["-vf", &format!("scale={}:{}", target_width, target_height)]);
        }

        cmd.arg("-");

        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn ffmpeg decoder. Is ffmpeg in PATH?")?;

        let frame_size = (target_width * target_height * 4) as usize;

        Ok(Self {
            child,
            frame_size,
            frame_index: 0,
            fps,
        })
    }

    pub fn decode_next(&mut self) -> Result<Option<Vec<u8>>> {
        let stdout = self.child.stdout.as_mut()
            .ok_or_else(|| anyhow::anyhow!("ffmpeg stdout not available"))?;

        let mut buf = vec![0u8; self.frame_size];
        let mut total_read = 0;

        while total_read < self.frame_size {
            match stdout.read(&mut buf[total_read..])? {
                0 => {
                    if total_read == 0 {
                        return Ok(None);
                    }
                    anyhow::bail!("Unexpected end of ffmpeg output mid-frame");
                }
                n => total_read += n,
            }
        }

        self.frame_index += 1;
        Ok(Some(buf))
    }

    #[allow(dead_code)]
    pub fn frame_pts_us(&self) -> u64 {
        ((self.frame_index as f64 / self.fps) * 1_000_000.0) as u64
    }

    pub fn finish(&mut self) -> Result<()> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(())
    }
}

pub struct VideoEncoder {
    child: Child,
    frame_size: usize,
}

impl VideoEncoder {
    pub fn create(
        path: &Path,
        width: u32,
        height: u32,
        fps_str: &str,
        encoder_name: &str,
        crf: u32,
        preset: &str,
        pixel_format: &str,
    ) -> Result<Self> {
        let size_str = format!("{}x{}", width, height);

        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error"])
            .args(["-y"])
            .args([
                "-f", "rawvideo",
                "-pix_fmt", "rgba",
                "-s", &size_str,
                "-r", fps_str,
                "-i", "pipe:0",
            ])
            .args(["-c:v", encoder_name])
            .args(["-crf", &crf.to_string()])
            .args(["-preset", preset])
            .args(["-pix_fmt", pixel_format])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn ffmpeg encoder. Is ffmpeg in PATH?")?;

        let frame_size = (width * height * 4) as usize;

        Ok(Self { child, frame_size })
    }

    pub fn encode_frame(&mut self, rgba_data: &[u8]) -> Result<()> {
        let stdin = self.child.stdin.as_mut()
            .ok_or_else(|| anyhow::anyhow!("ffmpeg stdin not available"))?;

        let write_len = rgba_data.len().min(self.frame_size);
        stdin.write_all(&rgba_data[..write_len])?;

        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        if let Some(stdin) = self.child.stdin.take() {
            drop(stdin);
        }

        let status = self.child.wait()
            .context("Failed to wait for ffmpeg encoder")?;

        if !status.success() {
            let stderr = self.child.stderr.as_mut()
                .map(|e| {
                    let mut buf = String::new();
                    let _ = e.read_to_string(&mut buf);
                    buf
                })
                .unwrap_or_default();
            anyhow::bail!("ffmpeg encoder failed: {}", stderr);
        }

        Ok(())
    }
}

pub fn mux_audio_video(
    video_path: &Path,
    source_path: &Path,
    output_path: &Path,
) -> Result<()> {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .args([
            "-i", &video_path.to_string_lossy(),
            "-i", &source_path.to_string_lossy(),
            "-c:v", "copy",
            "-c:a", "copy",
            "-map", "0:v:0",
            "-map", "1:a:0?",
            "-shortest",
            "-y", &output_path.to_string_lossy(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .context("Failed to run ffmpeg for muxing")?;

    if !status.success() {
        anyhow::bail!("ffmpeg muxing failed");
    }

    Ok(())
}
