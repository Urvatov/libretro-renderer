use std::time::Instant;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use librashader::presets::{ShaderFeatures, ShaderPreset};
use librashader::runtime::vk::{FilterChain, FilterChainOptions};
use librashader::runtime::Viewport;

use crate::cli::Args;
use crate::vulkan::HeadlessVulkan;
use crate::video::{self, VideoDecoder, VideoEncoder};

pub fn run(args: &Args) -> Result<()> {
    log::info!("Probing input video...");
    let info = video::probe_video(&args.input)?;
    log::info!(
        "Input: {}x{} @ {:.2} fps, ~{} frames",
        info.width,
        info.height,
        info.frame_rate,
        info.total_frames
    );

    let input_w = args.input_width.unwrap_or(info.width);
    let input_h = args.input_height.unwrap_or(info.height);
    let output_w = args.width.unwrap_or(input_w);
    let output_h = args.height.unwrap_or(input_h);

    log::info!("Initializing Vulkan (headless)...");
    let mut vk = HeadlessVulkan::new()?;

    log::info!("Loading shader preset: {}", args.shader.display());
    let preset = ShaderPreset::try_parse(&args.shader, ShaderFeatures::NONE)?;
    let vulkan_objects = vk.to_vulkan_objects();

    let mut filter_chain = unsafe {
        FilterChain::load_from_preset(
            preset,
            vulkan_objects,
            Some(&FilterChainOptions {
                frames_in_flight: 2,
                force_no_mipmaps: false,
                use_dynamic_rendering: false,
                disable_cache: true,
                ..Default::default()
            }),
        )
    }?;

    log::info!("Creating GPU images ({}x{})...", output_w, output_h);
    let input_image = vk.create_image(
        input_w,
        input_h,
        ash::vk::Format::R8G8B8A8_UNORM,
        ash::vk::ImageUsageFlags::TRANSFER_DST | ash::vk::ImageUsageFlags::SAMPLED,
    )?;
    let output_image = vk.create_image(
        output_w,
        output_h,
        ash::vk::Format::R8G8B8A8_UNORM,
        ash::vk::ImageUsageFlags::COLOR_ATTACHMENT | ash::vk::ImageUsageFlags::TRANSFER_SRC,
    )?;

    let input_pixels = (input_w * input_h * 4) as usize;
    let output_pixels = (output_w * output_h * 4) as usize;

    let (input_staging_buf, input_staging_alloc) =
        vk.create_staging_buffer(input_pixels as u64)?;
    let (output_staging_buf, output_staging_alloc) =
        vk.create_staging_buffer(output_pixels as u64)?;

    let cmd_upload = vk.allocate_command_buffer()?;
    let cmd_filter = vk.allocate_command_buffer()?;
    let cmd_download = vk.allocate_command_buffer()?;

    let fence = vk.create_fence()?;

    let mut decoder = VideoDecoder::open(&args.input, input_w, input_h, info.frame_rate)?;

    let temp_video = args.output.with_extension("tmp.mp4");
    let mut encoder = VideoEncoder::create(
        &temp_video,
        output_w,
        output_h,
        &info.frame_rate_str,
        &args.encoder,
        args.crf,
        &args.preset,
        &args.pixel_format,
    )?;

    let pb = ProgressBar::new(info.total_frames);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap(),
    );

    let start = Instant::now();
    let mut frame_count: u64 = 0;

    loop {
        match decoder.decode_next()? {
            Some(rgba_data) => {
                unsafe {
                    // Upload
                    let mapped = input_staging_alloc.mapped_ptr().unwrap().as_ptr() as *mut u8;
                    let copy_len = rgba_data.len().min(input_pixels);
                    std::ptr::copy_nonoverlapping(rgba_data.as_ptr(), mapped, copy_len);

                    vk.device.begin_command_buffer(
                        cmd_upload,
                        &ash::vk::CommandBufferBeginInfo::default()
                            .flags(ash::vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                    )?;
                    vk.record_upload_image(
                        cmd_upload,
                        input_staging_buf,
                        input_image.image,
                        input_w,
                        input_h,
                    );
                    vk.device.end_command_buffer(cmd_upload)?;

                    // Filter
                    vk.device.begin_command_buffer(
                        cmd_filter,
                        &ash::vk::CommandBufferBeginInfo::default()
                            .flags(ash::vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                    )?;
                    filter_chain.frame(
                        &input_image,
                        &Viewport::new_render_target_sized_origin(
                            output_image.clone(),
                            None,
                        )?,
                        cmd_filter,
                        frame_count as usize,
                        None,
                    )?;
                    vk.device.end_command_buffer(cmd_filter)?;

                    // Download
                    vk.device.begin_command_buffer(
                        cmd_download,
                        &ash::vk::CommandBufferBeginInfo::default()
                            .flags(ash::vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                    )?;
                    vk.record_download_image(
                        cmd_download,
                        output_image.image,
                        output_staging_buf,
                        output_w,
                        output_h,
                    );
                    vk.device.end_command_buffer(cmd_download)?;

                    // Submit all
                    let cmds = [cmd_upload, cmd_filter, cmd_download];
                    let submit_info = ash::vk::SubmitInfo::default()
                        .command_buffers(&cmds);
                    vk.device.queue_submit(
                        vk.queue,
                        &[submit_info],
                        fence,
                    )?;
                    vk.device.wait_for_fences(&[fence], true, u64::MAX)?;
                    vk.device.reset_fences(&[fence])?;
                }

                // Read back and encode
                unsafe {
                    let mapped = output_staging_alloc.mapped_ptr().unwrap().as_ptr() as *const u8;
                    let frame_data =
                        std::slice::from_raw_parts(mapped, output_pixels);
                    encoder.encode_frame(frame_data)?;
                }

                frame_count += 1;
                pb.set_position(frame_count);
            }
            None => break,
        }
    }

    pb.finish_with_message("done");
    decoder.finish()?;
    encoder.finish()?;

    let elapsed = start.elapsed().as_secs_f64();
    let avg_fps = frame_count as f64 / elapsed;
    log::info!(
        "Rendered {} frames in {:.1}s ({:.1} fps)",
        frame_count,
        elapsed,
        avg_fps
    );

    // Mux with audio from original input
    log::info!("Muxing audio from source...");
    match video::mux_audio_video(&temp_video, &args.input, &args.output) {
        Ok(()) => {
            let _ = std::fs::remove_file(&temp_video);
        }
        Err(e) => {
            log::warn!("Audio muxing failed ({}), keeping video-only output", e);
            let _ = std::fs::rename(&temp_video, &args.output);
        }
    }
    log::info!("Done: {}", args.output.display());

    Ok(())
}
