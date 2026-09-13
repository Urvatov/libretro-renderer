use ash::{vk, Device, Entry, Instance};
use gpu_allocator::vulkan::{Allocator, AllocatorCreateDesc};
use librashader::runtime::vk::VulkanImage;

pub struct HeadlessVulkan {
    #[allow(dead_code)]
    pub entry: Entry,
    pub instance: Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: Device,
    pub queue: vk::Queue,
    #[allow(dead_code)]
    pub queue_family_index: u32,
    pub allocator: Allocator,
    pub command_pool: vk::CommandPool,
}

impl HeadlessVulkan {
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            let entry = Entry::load()?;

            let app_info = vk::ApplicationInfo::default()
                .api_version(vk::make_api_version(0, 1, 3, 0));
            let create_info = vk::InstanceCreateInfo::default()
                .application_info(&app_info)
                .enabled_extension_names(&[]);

            let instance = entry.create_instance(&create_info, None)?;

            let physical_devices = instance.enumerate_physical_devices()?;
            let physical_device = physical_devices
                .into_iter()
                .max_by_key(|&pd| {
                    let props = instance.get_physical_device_properties(pd);
                    match props.device_type {
                        vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                        vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                        vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                        _ => 0,
                    }
                })
                .ok_or_else(|| anyhow::anyhow!("No Vulkan GPU found"))?;

            let queue_family_index = find_queue_family(&instance, physical_device)?;

            let queue_priority = [1.0f32];
            let queue_info = vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family_index)
                .queue_priorities(&queue_priority);

            let features = vk::PhysicalDeviceFeatures::default();
            let device_create_info = vk::DeviceCreateInfo::default()
                .queue_create_infos(std::slice::from_ref(&queue_info))
                .enabled_extension_names(&[])
                .enabled_features(&features);

            let device = instance.create_device(physical_device, &device_create_info, None)?;
            let queue = device.get_device_queue(queue_family_index, 0);

            let mut debug_settings = gpu_allocator::AllocatorDebugSettings::default();
            debug_settings.log_leaks_on_shutdown = false;

            let allocator = Allocator::new(&AllocatorCreateDesc {
                instance: instance.clone(),
                physical_device,
                device: device.clone(),
                debug_settings,
                buffer_device_address: false,
                allocation_sizes: Default::default(),
            })?;

            let pool_info = vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(queue_family_index);
            let command_pool = device.create_command_pool(&pool_info, None)?;

            Ok(Self {
                entry,
                instance,
                physical_device,
                device,
                queue,
                queue_family_index,
                allocator,
                command_pool,
            })
        }
    }

    pub fn to_vulkan_objects(&self) -> (vk::PhysicalDevice, ash::Instance, ash::Device, vk::Queue) {
        (
            self.physical_device,
            self.instance.clone(),
            self.device.clone(),
            self.queue,
        )
    }

    pub fn create_image(
        &mut self,
        width: u32,
        height: u32,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
    ) -> anyhow::Result<VulkanImage> {
        unsafe {
            let image_info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED);

            let image = self.device.create_image(&image_info, None)?;

            let mem_reqs = self.device.get_image_memory_requirements(image);
            let alloc_info = gpu_allocator::vulkan::AllocationCreateDesc {
                name: "vulkan-image",
                requirements: mem_reqs,
                location: gpu_allocator::MemoryLocation::GpuOnly,
                linear: false,
                allocation_scheme: gpu_allocator::vulkan::AllocationScheme::GpuAllocatorManaged,
            };
            let allocation = self.allocator.allocate(&alloc_info)?;

            self.device
                .bind_image_memory(image, allocation.memory(), allocation.offset())?;

            Ok(VulkanImage {
                image,
                size: librashader::runtime::Size {
                    width,
                    height,
                },
                format,
            })
        }
    }

    pub fn create_staging_buffer(&mut self, size: vk::DeviceSize) -> anyhow::Result<(vk::Buffer, gpu_allocator::vulkan::Allocation)> {
        unsafe {
            let buffer_info = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);

            let buffer = self.device.create_buffer(&buffer_info, None)?;
            let mem_reqs = self.device.get_buffer_memory_requirements(buffer);
            let alloc_info = gpu_allocator::vulkan::AllocationCreateDesc {
                name: "staging-buffer",
                requirements: mem_reqs,
                location: gpu_allocator::MemoryLocation::CpuToGpu,
                linear: true,
                allocation_scheme: gpu_allocator::vulkan::AllocationScheme::GpuAllocatorManaged,
            };
            let allocation = self.allocator.allocate(&alloc_info)?;
            self.device
                .bind_buffer_memory(buffer, allocation.memory(), allocation.offset())?;

            Ok((buffer, allocation))
        }
    }

    pub fn allocate_command_buffer(&self) -> anyhow::Result<vk::CommandBuffer> {
        unsafe {
            let alloc_info = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);

            let buffers = self.device.allocate_command_buffers(&alloc_info)?;
            Ok(buffers[0])
        }
    }

    pub fn create_fence(&self) -> anyhow::Result<vk::Fence> {
        unsafe {
            let fence_info = vk::FenceCreateInfo::default()
                .flags(vk::FenceCreateFlags::SIGNALED);
            Ok(self.device.create_fence(&fence_info, None)?)
        }
    }

    /// Record: copy staging buffer -> image, transition to SHADER_READ_ONLY_OPTIMAL
    pub fn record_upload_image(
        &self,
        cmd: vk::CommandBuffer,
        src_buffer: vk::Buffer,
        dst_image: vk::Image,
        width: u32,
        height: u32,
    ) {
        unsafe {
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(dst_image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });

            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );

            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                });

            self.device.cmd_copy_buffer_to_image(
                cmd,
                src_buffer,
                dst_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );

            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(dst_image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });

            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
    }

    /// Record: transition output image to TRANSFER_SRC, copy to staging buffer
    pub fn record_download_image(
        &self,
        cmd: vk::CommandBuffer,
        src_image: vk::Image,
        dst_buffer: vk::Buffer,
        width: u32,
        height: u32,
    ) {
        unsafe {
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(src_image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });

            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );

            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                });

            self.device.cmd_copy_image_to_buffer(
                cmd,
                src_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst_buffer,
                &[region],
            );
        }
    }

    #[allow(dead_code)]
    pub fn wait_idle(&self) {
        unsafe {
            let _ = self.device.device_wait_idle();
        }
    }
}

impl Drop for HeadlessVulkan {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn find_queue_family(
    instance: &Instance,
    physical_device: vk::PhysicalDevice,
) -> anyhow::Result<u32> {
    let queue_families = unsafe { instance.get_physical_device_queue_family_properties(physical_device) };

    for (index, family) in queue_families.iter().enumerate() {
        if family.queue_flags.contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE) {
            return Ok(index as u32);
        }
    }

    for (index, family) in queue_families.iter().enumerate() {
        if family.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
            return Ok(index as u32);
        }
    }

    Err(anyhow::anyhow!("No suitable queue family found"))
}
