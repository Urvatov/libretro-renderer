use std::time::Instant;

use anyhow::{Context, Result};
use ash::vk;
use gpu_allocator::vulkan::Allocation;
use indicatif::{ProgressBar, ProgressStyle};
use librashader::presets::{ShaderFeatures, ShaderPreset};
use librashader::runtime::Viewport;
use librashader::runtime::vk::{FilterChain, FilterChainOptions, VulkanImage};

use crate::cli::Args;
use crate::video::{self, VideoDecoder, VideoEncoder};
use crate::vulkan::HeadlessVulkan;

struct FrameResources {
    input_image: VulkanImage,
    output_image: VulkanImage,
    input_staging_buf: vk::Buffer,
    input_staging_alloc: Allocation,
    output_staging_buf: vk::Buffer,
    output_staging_alloc: Allocation,
    cmd_upload: vk::CommandBuffer,
    cmd_filter: vk::CommandBuffer,
    cmd_download: vk::CommandBuffer,
    fence: vk::Fence,
}

pub fn run(args: &Args) -> Result<()> {
    if video::is_image_path(&args.input) {
        render_image(args)
    } else {
        render_video(args)
    }
}

fn render_image(args: &Args) -> Result<()> {
    log::info!("Loading input image: {}", args.input.display());
    let mut img = image::open(&args.input)
        .with_context(|| format!("Failed to open input image '{}'", args.input.display()))?
        .to_rgba8();

    let (img_w, img_h) = img.dimensions();
    log::info!("Input image size: {}x{}", img_w, img_h);

    let input_w = args.input_width.unwrap_or(img_w);
    let input_h = args.input_height.unwrap_or(img_h);

    if input_w != img_w || input_h != img_h {
        log::info!("Downscaling input image to {}x{}...", input_w, input_h);
        img = image::imageops::resize(
            &img,
            input_w,
            input_h,
            image::imageops::FilterType::Lanczos3,
        );
    }

    let output_w = args.width.unwrap_or(input_w);
    let output_h = args.height.unwrap_or(input_h);
    log::info!("Target output size: {}x{}", output_w, output_h);

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
                frames_in_flight: 1,
                force_no_mipmaps: false,
                use_dynamic_rendering: false,
                disable_cache: true,
                ..Default::default()
            }),
        )
    }?;

    let input_pixels = (input_w * input_h * 4) as usize;
    let output_pixels = (output_w * output_h * 4) as usize;

    let input_image = vk.create_image(
        input_w,
        input_h,
        vk::Format::R8G8B8A8_UNORM,
        vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
    )?;
    let output_image = vk.create_image(
        output_w,
        output_h,
        vk::Format::R8G8B8A8_UNORM,
        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
    )?;
    let (input_staging_buf, input_staging_alloc) =
        vk.create_staging_buffer(input_pixels as u64, gpu_allocator::MemoryLocation::CpuToGpu)?;
    let (output_staging_buf, output_staging_alloc) = vk.create_staging_buffer(
        output_pixels as u64,
        gpu_allocator::MemoryLocation::GpuToCpu,
    )?;

    let cmd_upload = vk.allocate_command_buffer()?;
    let cmd_filter = vk.allocate_command_buffer()?;
    let cmd_download = vk.allocate_command_buffer()?;
    let fence = vk.create_fence()?;

    let start = Instant::now();
    log::info!("Rendering shader on image...");

    unsafe {
        // Upload image to staging buffer
        let mapped = input_staging_alloc.mapped_ptr().unwrap().as_ptr() as *mut u8;
        let raw_bytes = img.as_raw();
        let copy_len = raw_bytes.len().min(input_pixels);
        std::ptr::copy_nonoverlapping(raw_bytes.as_ptr(), mapped, copy_len);

        let range = vk::MappedMemoryRange::default()
            .memory(input_staging_alloc.memory())
            .offset(input_staging_alloc.offset())
            .size(vk::WHOLE_SIZE);
        let _ = vk.device.flush_mapped_memory_ranges(&[range]);

        // Record upload commands
        vk.device.begin_command_buffer(
            cmd_upload,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        vk.record_upload_image(
            cmd_upload,
            input_staging_buf,
            input_image.image,
            input_w,
            input_h,
        );
        vk.device.end_command_buffer(cmd_upload)?;

        // Record shader filter commands
        vk.device.begin_command_buffer(
            cmd_filter,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        filter_chain.frame(
            &input_image,
            &Viewport::new_render_target_sized_origin(output_image.clone(), None)?,
            cmd_filter,
            0,
            None,
        )?;
        vk.device.end_command_buffer(cmd_filter)?;

        // Record download commands
        vk.device.begin_command_buffer(
            cmd_download,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        vk.record_download_image(
            cmd_download,
            output_image.image,
            output_staging_buf,
            output_w,
            output_h,
        );
        vk.device.end_command_buffer(cmd_download)?;

        // Submit GPU work
        let cmds = [cmd_upload, cmd_filter, cmd_download];
        let submit_info = vk::SubmitInfo::default().command_buffers(&cmds);
        vk.device.queue_submit(vk.queue, &[submit_info], fence)?;

        // Wait for fence
        vk.device.wait_for_fences(&[fence], true, u64::MAX)?;

        // Invalidate output memory range for CPU read
        let range = vk::MappedMemoryRange::default()
            .memory(output_staging_alloc.memory())
            .offset(output_staging_alloc.offset())
            .size(vk::WHOLE_SIZE);
        let _ = vk.device.invalidate_mapped_memory_ranges(&[range]);
    }

    // Determine target output path
    let output_path = if video::is_image_path(&args.output) {
        args.output.clone()
    } else if args.output.extension().is_none() {
        args.output.with_extension("png")
    } else {
        log::warn!(
            "Input is an image but output '{}' has a non-image extension. Outputting as PNG.",
            args.output.display()
        );
        args.output.with_extension("png")
    };

    unsafe {
        let mapped = output_staging_alloc.mapped_ptr().unwrap().as_ptr() as *const u8;
        let slice = std::slice::from_raw_parts(mapped, output_pixels);

        let out_img = image::RgbaImage::from_raw(output_w, output_h, slice.to_vec())
            .ok_or_else(|| anyhow::anyhow!("Failed to create RGBA image from buffer"))?;
        out_img.save(&output_path)?;

        vk.wait_idle();
        vk.device.destroy_fence(fence, None);
        vk.device.destroy_buffer(input_staging_buf, None);
        vk.device.destroy_buffer(output_staging_buf, None);
    }

    let elapsed = start.elapsed().as_secs_f64();
    log::info!(
        "Image processed in {:.2}s. Saved to {}",
        elapsed,
        output_path.display()
    );
    println!("Saved rendered image to: {}", output_path.display());

    Ok(())
}

fn render_video(args: &Args) -> Result<()> {
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
    let batch_size = (args.batch_size as usize).max(1);

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
                frames_in_flight: batch_size as u32,
                force_no_mipmaps: false,
                use_dynamic_rendering: false,
                disable_cache: true,
                ..Default::default()
            }),
        )
    }?;

    log::info!(
        "Creating {} frame resource sets ({}x{})...",
        batch_size,
        output_w,
        output_h
    );

    let input_pixels = (input_w * input_h * 4) as usize;
    let output_pixels = (output_w * output_h * 4) as usize;

    let mut frames: Vec<FrameResources> = Vec::with_capacity(batch_size);
    for _ in 0..batch_size {
        let input_image = vk.create_image(
            input_w,
            input_h,
            vk::Format::R8G8B8A8_UNORM,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
        )?;
        let output_image = vk.create_image(
            output_w,
            output_h,
            vk::Format::R8G8B8A8_UNORM,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
        )?;
        // Input staging buffer: CPU writes, GPU reads (CpuToGpu)
        let (input_staging_buf, input_staging_alloc) =
            vk.create_staging_buffer(input_pixels as u64, gpu_allocator::MemoryLocation::CpuToGpu)?;
        // Output staging buffer: GPU writes, CPU reads (GpuToCpu - HOST_CACHED!)
        let (output_staging_buf, output_staging_alloc) = vk.create_staging_buffer(
            output_pixels as u64,
            gpu_allocator::MemoryLocation::GpuToCpu,
        )?;
        let cmd_upload = vk.allocate_command_buffer()?;
        let cmd_filter = vk.allocate_command_buffer()?;
        let cmd_download = vk.allocate_command_buffer()?;
        let fence = vk.create_fence()?;

        frames.push(FrameResources {
            input_image,
            output_image,
            input_staging_buf,
            input_staging_alloc,
            output_staging_buf,
            output_staging_alloc,
            cmd_upload,
            cmd_filter,
            cmd_download,
            fence,
        });
    }

    let mut decoder = VideoDecoder::open(
        &args.input,
        input_w,
        input_h,
        info.frame_rate,
        &args.hwaccel,
        batch_size,
    )?;

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
        batch_size,
    )?;

    let pb = ProgressBar::new(info.total_frames);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
            )
            .unwrap(),
    );

    let start = Instant::now();
    let mut submitted_count: u64 = 0;
    let mut encoded_count: u64 = 0;

    loop {
        // Decode next frame from threaded reader
        let rgba_data = match decoder.decode_next()? {
            Some(data) => data,
            None => break,
        };

        // If ring buffer is full, wait for and encode the oldest in-flight frame
        if submitted_count - encoded_count == batch_size as u64 {
            let slot = (encoded_count as usize) % batch_size;
            let fr = &mut frames[slot];

            unsafe {
                vk.device.wait_for_fences(&[fr.fence], true, u64::MAX)?;
                vk.device.reset_fences(&[fr.fence])?;

                // Invalidate host cache for output staging buffer
                let range = vk::MappedMemoryRange::default()
                    .memory(fr.output_staging_alloc.memory())
                    .offset(fr.output_staging_alloc.offset())
                    .size(vk::WHOLE_SIZE);
                let _ = vk.device.invalidate_mapped_memory_ranges(&[range]);

                let mapped = fr.output_staging_alloc.mapped_ptr().unwrap().as_ptr() as *const u8;
                let frame_slice = std::slice::from_raw_parts(mapped, output_pixels);
                encoder.encode_frame(frame_slice.to_vec())?;
            }

            encoded_count += 1;
        }

        // Slot for submitted_count is now guaranteed free
        let slot = (submitted_count as usize) % batch_size;
        let fr = &mut frames[slot];

        unsafe {
            // Upload: copy decoded frame into staging buffer
            let mapped = fr.input_staging_alloc.mapped_ptr().unwrap().as_ptr() as *mut u8;
            let copy_len = rgba_data.len().min(input_pixels);
            std::ptr::copy_nonoverlapping(rgba_data.as_ptr(), mapped, copy_len);

            let range = vk::MappedMemoryRange::default()
                .memory(fr.input_staging_alloc.memory())
                .offset(fr.input_staging_alloc.offset())
                .size(vk::WHOLE_SIZE);
            let _ = vk.device.flush_mapped_memory_ranges(&[range]);

            // Record upload commands
            vk.device.begin_command_buffer(
                fr.cmd_upload,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            vk.record_upload_image(
                fr.cmd_upload,
                fr.input_staging_buf,
                fr.input_image.image,
                input_w,
                input_h,
            );
            vk.device.end_command_buffer(fr.cmd_upload)?;

            // Record shader filter
            vk.device.begin_command_buffer(
                fr.cmd_filter,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            filter_chain.frame(
                &fr.input_image,
                &Viewport::new_render_target_sized_origin(fr.output_image.clone(), None)?,
                fr.cmd_filter,
                submitted_count as usize,
                None,
            )?;
            vk.device.end_command_buffer(fr.cmd_filter)?;

            // Record download commands
            vk.device.begin_command_buffer(
                fr.cmd_download,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            vk.record_download_image(
                fr.cmd_download,
                fr.output_image.image,
                fr.output_staging_buf,
                output_w,
                output_h,
            );
            vk.device.end_command_buffer(fr.cmd_download)?;

            // Submit all GPU work for this frame
            let cmds = [fr.cmd_upload, fr.cmd_filter, fr.cmd_download];
            let submit_info = vk::SubmitInfo::default().command_buffers(&cmds);
            vk.device.queue_submit(vk.queue, &[submit_info], fr.fence)?;
        }

        submitted_count += 1;
        pb.set_position(submitted_count);
    }

    // Drain all remaining in-flight frames
    while encoded_count < submitted_count {
        let slot = (encoded_count as usize) % batch_size;
        let fr = &mut frames[slot];

        unsafe {
            vk.device.wait_for_fences(&[fr.fence], true, u64::MAX)?;
            vk.device.reset_fences(&[fr.fence])?;

            let range = vk::MappedMemoryRange::default()
                .memory(fr.output_staging_alloc.memory())
                .offset(fr.output_staging_alloc.offset())
                .size(vk::WHOLE_SIZE);
            let _ = vk.device.invalidate_mapped_memory_ranges(&[range]);

            let mapped = fr.output_staging_alloc.mapped_ptr().unwrap().as_ptr() as *const u8;
            let frame_slice = std::slice::from_raw_parts(mapped, output_pixels);
            encoder.encode_frame(frame_slice.to_vec())?;
        }

        encoded_count += 1;
    }

    pb.finish_with_message("done");
    decoder.finish()?;
    encoder.finish()?;

    let elapsed = start.elapsed().as_secs_f64();
    let avg_fps = if elapsed > 0.0 {
        encoded_count as f64 / elapsed
    } else {
        0.0
    };
    log::info!(
        "Rendered {} frames in {:.2}s ({:.1} fps, batch_size={})",
        encoded_count,
        elapsed,
        avg_fps,
        batch_size
    );
    println!(
        "Rendered {} frames in {:.2}s ({:.1} fps)",
        encoded_count, elapsed, avg_fps
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

    // Clean up Vulkan resources
    unsafe {
        vk.wait_idle();
        for fr in &frames {
            vk.device.destroy_fence(fr.fence, None);
            vk.device.destroy_buffer(fr.input_staging_buf, None);
            vk.device.destroy_buffer(fr.output_staging_buf, None);
        }
    }

    Ok(())
}
