#![allow(clippy::len_zero)]

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

#[cfg(feature = "dx12")]
use gfx_backend_dx12 as back;
#[cfg(feature = "metal")]
use gfx_backend_metal as back;
#[cfg(feature = "vulkan")]
use gfx_backend_vulkan as back;

use arrayvec::ArrayVec;
use core::mem::ManuallyDrop;
use gfx_hal::{
  adapter::{Adapter, PhysicalDevice},
  command::{ClearColor, ClearValue, CommandBuffer, MultiShot, Primary},
  device::Device,
  error::HostExecutionError,
  format::{Aspects, ChannelType, Format, Swizzle},
  image::{Extent, Layout, SubresourceRange, Usage, ViewKind},
  pass::{Attachment, AttachmentLoadOp, AttachmentOps, AttachmentStoreOp, SubpassDesc},
  pool::{CommandPool, CommandPoolCreateFlags},
  pso::{PipelineStage, Rect},
  queue::{family::QueueGroup, Submission},
  window::{Backbuffer, FrameSync, PresentMode, Swapchain, SwapchainConfig},
  Backend, Gpu, Graphics, Instance, QueueFamily, Surface,
};
use winit::{dpi::LogicalSize, CreationError, Event, EventsLoop, Window, WindowBuilder, WindowEvent};

pub const WINDOW_NAME: &str = "Hello Clear";

pub struct HalState {
  current_frame: usize,
  frames_in_flight: usize,
  in_flight_fences: Vec<<back::Backend as Backend>::Fence>,
  render_finished_semaphores: Vec<<back::Backend as Backend>::Semaphore>,
  image_available_semaphores: Vec<<back::Backend as Backend>::Semaphore>,
  command_buffers: Vec<CommandBuffer<back::Backend, Graphics, MultiShot, Primary>>,
  command_pool: ManuallyDrop<CommandPool<back::Backend, Graphics>>,
  framebuffers: Vec<<back::Backend as Backend>::Framebuffer>,
  image_views: Vec<(<back::Backend as Backend>::ImageView)>,
  render_pass: ManuallyDrop<<back::Backend as Backend>::RenderPass>,
  render_area: Rect,
  queue_group: QueueGroup<back::Backend, Graphics>,
  swapchain: ManuallyDrop<<back::Backend as Backend>::Swapchain>,
  device: ManuallyDrop<back::Device>,
  _adapter: Adapter<back::Backend>,
  _surface: <back::Backend as Backend>::Surface,
  _instance: ManuallyDrop<back::Instance>,
}
impl HalState {
  pub fn new(window: &Window) -> Result<Self, &'static str> {
    // Create An Instance
    let instance = back::Instance::create(WINDOW_NAME, 1);

    // Create A Surface
    let mut surface = instance.create_surface(window);

    // Select An Adapter
    let adapter = instance
      .enumerate_adapters()
      .into_iter()
      .find(|a| {
        a.queue_families
          .iter()
          .any(|qf| qf.supports_graphics() && qf.max_queues() > 0 && surface.supports_queue_family(qf))
      })
      .ok_or("Couldn't find a graphical Adapter!")?;

    // Open A Device and take out a QueueGroup
    let (device, queue_group) = {
      let queue_family = adapter
        .queue_families
        .iter()
        .find(|qf| qf.supports_graphics() && qf.max_queues() > 0 && surface.supports_queue_family(qf))
        .ok_or("Couldn't find a QueueFamily with graphics!")?;
      let Gpu { device, mut queues } = unsafe {
        adapter
          .physical_device
          .open(&[(&queue_family, &[1.0; 1])])
          .map_err(|_| "Couldn't open the PhysicalDevice!")?
      };
      let queue_group = queues
        .take::<Graphics>(queue_family.id())
        .ok_or("Couldn't take ownership of the QueueGroup!")?;
      let _ = if queue_group.queues.len() > 0 {
        Ok(())
      } else {
        Err("The QueueGroup did not have any CommandQueues available!")
      }?;
      (device, queue_group)
    };

    // Create A Swapchain, this is extra long
    let (swapchain, extent, backbuffer, format, frames_in_flight) = {
      let (caps, preferred_formats, present_modes, composite_alphas) = surface.compatibility(&adapter.physical_device);
      info!("{:?}", caps);
      info!("Preferred Formats: {:?}", preferred_formats);
      info!("Present Modes: {:?}", present_modes);
      info!("Composite Alphas: {:?}", composite_alphas);
      //
      let present_mode = {
        use gfx_hal::window::PresentMode::*;
        [Mailbox, Fifo, Relaxed, Immediate]
          .iter()
          .cloned()
          .find(|pm| present_modes.contains(pm))
          .ok_or("No PresentMode values specified!")?
      };
      let composite_alpha = {
        use gfx_hal::window::CompositeAlpha::*;
        [Opaque, Inherit, PreMultiplied, PostMultiplied]
          .iter()
          .cloned()
          .find(|ca| composite_alphas.contains(ca))
          .ok_or("No CompositeAlpha values specified!")?
      };
      let format = match preferred_formats {
        None => Format::Rgba8Srgb,
        Some(formats) => match formats
          .iter()
          .find(|format| format.base_format().1 == ChannelType::Srgb)
          .cloned()
        {
          Some(srgb_format) => srgb_format,
          None => formats.get(0).cloned().ok_or("Preferred format list was empty!")?,
        },
      };
      let extent = caps.extents.end;
      let image_count = if present_mode == PresentMode::Mailbox {
        (caps.image_count.end - 1).min(3)
      } else {
        (caps.image_count.end - 1).min(2)
      };
      let image_layers = 1;
      let image_usage = if caps.usage.contains(Usage::COLOR_ATTACHMENT) {
        Usage::COLOR_ATTACHMENT
      } else {
        Err("The Surface isn't capable of supporting color!")?
      };
      let swapchain_config = SwapchainConfig {
        present_mode,
        composite_alpha,
        format,
        extent,
        image_count,
        image_layers,
        image_usage,
      };
      info!("{:?}", swapchain_config);
      //
      let (swapchain, backbuffer) = unsafe {
        device
          .create_swapchain(&mut surface, swapchain_config, None)
          .map_err(|_| "Failed to create the swapchain!")?
      };
      (swapchain, extent, backbuffer, format, image_count as usize)
    };

    // Define A RenderPass
    let render_pass = {
      let color_attachment = Attachment {
        format: Some(format),
        samples: 1,
        ops: AttachmentOps {
          load: AttachmentLoadOp::Clear,
          store: AttachmentStoreOp::Store,
        },
        stencil_ops: AttachmentOps::DONT_CARE,
        layouts: Layout::Undefined..Layout::Present,
      };
      let subpass = SubpassDesc {
        colors: &[(0, Layout::ColorAttachmentOptimal)],
        depth_stencil: None,
        inputs: &[],
        resolves: &[],
        preserves: &[],
      };
      unsafe {
        device
          .create_render_pass(&[color_attachment], &[subpass], &[])
          .map_err(|_| "Couldn't create a render pass!")?
      }
    };

    // Create The ImageViews
    let image_views: Vec<_> = match backbuffer {
      Backbuffer::Images(images) => images
        .into_iter()
        .map(|image| unsafe {
          device
            .create_image_view(
              &image,
              ViewKind::D2,
              format,
              Swizzle::NO,
              SubresourceRange {
                aspects: Aspects::COLOR,
                levels: 0..1,
                layers: 0..1,
              },
            )
            .map_err(|_| "Couldn't create the image_view for the image!")
        })
        .collect::<Result<Vec<_>, &str>>()?,
      Backbuffer::Framebuffer(_) => unimplemented!("Can't handle framebuffer backbuffer!"),
    };

    // Create Our FrameBuffers
    let framebuffers: Vec<<back::Backend as Backend>::Framebuffer> = {
      image_views
        .iter()
        .map(|image_view| unsafe {
          device
            .create_framebuffer(
              &render_pass,
              vec![image_view],
              Extent {
                width: extent.width as u32,
                height: extent.height as u32,
                depth: 1,
              },
            )
            .map_err(|_| "Failed to create a framebuffer!")
        })
        .collect::<Result<Vec<_>, &str>>()?
    };

    // Create Our CommandPool
    let mut command_pool = unsafe {
      device
        .create_command_pool_typed(&queue_group, CommandPoolCreateFlags::RESET_INDIVIDUAL)
        .map_err(|_| "Could not create the raw command pool!")?
    };

    // Create Our CommandBuffers
    let command_buffers: Vec<_> = framebuffers.iter().map(|_| command_pool.acquire_command_buffer()).collect();

    // Create Our Sync Primitives
    let (image_available_semaphores, render_finished_semaphores, in_flight_fences) = {
      let mut image_available_semaphores: Vec<<back::Backend as Backend>::Semaphore> = vec![];
      let mut render_finished_semaphores: Vec<<back::Backend as Backend>::Semaphore> = vec![];
      let mut in_flight_fences: Vec<<back::Backend as Backend>::Fence> = vec![];
      for _ in 0..command_buffers.len() {
        image_available_semaphores.push(device.create_semaphore().map_err(|_| "Could not create a semaphore!")?);
        render_finished_semaphores.push(device.create_semaphore().map_err(|_| "Could not create a semaphore!")?);
        in_flight_fences.push(device.create_fence(true).map_err(|_| "Could not create a fence!")?);
      }
      (image_available_semaphores, render_finished_semaphores, in_flight_fences)
    };

    Ok(Self {
      _instance: ManuallyDrop::new(instance),
      _surface: surface,
      _adapter: adapter,
      device: ManuallyDrop::new(device),
      queue_group,
      swapchain: ManuallyDrop::new(swapchain),
      render_area: extent.to_extent().rect(),
      render_pass: ManuallyDrop::new(render_pass),
      image_views,
      framebuffers,
      command_pool: ManuallyDrop::new(command_pool),
      command_buffers,
      image_available_semaphores,
      render_finished_semaphores,
      in_flight_fences,
      frames_in_flight,
      current_frame: 0,
    })
  }

  /// Draw a frame that's just cleared to the color specified.
  pub fn draw_clear_frame(&mut self, color: [f32; 4]) -> Result<(), &'static str> {
    // SETUP FOR THIS FRAME
    let flight_fence = &self.in_flight_fences[self.current_frame];
    let image_available = &self.image_available_semaphores[self.current_frame];
    let render_finished = &self.render_finished_semaphores[self.current_frame];
    // Advance the frame _before_ we start using the `?` operator
    self.current_frame = (self.current_frame + 1) % self.frames_in_flight;

    let (i_u32, i_usize) = unsafe {
      self
        .device
        .wait_for_fence(flight_fence, core::u64::MAX)
        .map_err(|_| "Failed to wait on the fence!")?;
      self
        .device
        .reset_fence(flight_fence)
        .map_err(|_| "Couldn't reset the fence!")?;
      let image_index = self
        .swapchain
        .acquire_image(core::u64::MAX, FrameSync::Semaphore(image_available))
        .map_err(|_| "Couldn't acquire an image from the swapchain!")?;
      (image_index, image_index as usize)
    };

    // RECORD COMMANDS
    unsafe {
      let buffer = &mut self.command_buffers[i_usize];
      let clear_values = [ClearValue::Color(ClearColor::Float(color))];
      buffer.begin(false);
      buffer.begin_render_pass_inline(
        &self.render_pass,
        &self.framebuffers[i_usize],
        self.render_area,
        clear_values.iter(),
      );
      buffer.finish();
    }

    // SUBMISSION AND PRESENT
    let command_buffers = &self.command_buffers[i_usize..=i_usize];
    let wait_semaphores: ArrayVec<[_; 1]> = [(image_available, PipelineStage::COLOR_ATTACHMENT_OUTPUT)].into();
    let signal_semaphores: ArrayVec<[_; 1]> = [render_finished].into();
    // yes, you have to write it twice like this. yes, it's silly.
    let present_wait_semaphores: ArrayVec<[_; 1]> = [render_finished].into();
    let submission = Submission {
      command_buffers,
      wait_semaphores,
      signal_semaphores,
    };
    let the_command_queue = &mut self.queue_group.queues[0];
    unsafe {
      the_command_queue.submit(submission, Some(flight_fence));
      self
        .swapchain
        .present(the_command_queue, i_u32, present_wait_semaphores)
        .map_err(|_| "Failed to present into the swapchain!")
    }
  }

  /// Waits until the device goes idle.
  pub fn wait_until_idle(&self) -> Result<(), HostExecutionError> {
    self.device.wait_idle()
  }
}
impl core::ops::Drop for HalState {
  /// We have to clean up "leaf" elements before "root" elements. Basically, we
  /// clean up in reverse of the order that we created things.
  fn drop(&mut self) {
    unsafe {
      for fence in self.in_flight_fences.drain(..) {
        self.device.destroy_fence(fence)
      }
      for semaphore in self.render_finished_semaphores.drain(..) {
        self.device.destroy_semaphore(semaphore)
      }
      for semaphore in self.image_available_semaphores.drain(..) {
        self.device.destroy_semaphore(semaphore)
      }
      for framebuffer in self.framebuffers.drain(..) {
        self.device.destroy_framebuffer(framebuffer);
      }
      for image_view in self.image_views.drain(..) {
        self.device.destroy_image_view(image_view);
      }
      // LAST RESORT STYLE CODE, NOT TO BE IMITATED LIGHTLY
      use core::ptr::read;
      self
        .device
        .destroy_command_pool(ManuallyDrop::into_inner(read(&mut self.command_pool)).into_raw());
      self
        .device
        .destroy_render_pass(ManuallyDrop::into_inner(read(&mut self.render_pass)));
      self
        .device
        .destroy_swapchain(ManuallyDrop::into_inner(read(&mut self.swapchain)));
      ManuallyDrop::drop(&mut self.device);
      ManuallyDrop::drop(&mut self._instance);
    }
  }
}

#[derive(Debug)]
pub struct WinitState {
  pub events_loop: EventsLoop,
  pub window: Window,
}
impl WinitState {
  /// Constructs a new `EventsLoop` and `Window` pair.
  ///
  /// The specified title and size are used, other elements are default.
  /// ## Failure
  /// It's possible for the window creation to fail. This is unlikely.
  pub fn new<T: Into<String>>(title: T, size: LogicalSize) -> Result<Self, CreationError> {
    let events_loop = EventsLoop::new();
    let output = WindowBuilder::new()
      .with_title(title)
      .with_dimensions(size)
      .build(&events_loop);
    output.map(|window| Self { events_loop, window })
  }
}
impl Default for WinitState {
  /// Makes an 800x600 window with the `WINDOW_NAME` value as the title.
  /// ## Panics
  /// If a `CreationError` occurs.
  fn default() -> Self {
    Self::new(
      WINDOW_NAME,
      LogicalSize {
        width: 800.0,
        height: 600.0,
      },
    )
    .expect("Could not create a window!")
  }
}

fn main() {
  simple_logger::init().unwrap();

  let mut winit_state = WinitState::default();

  let mut hal_state = match HalState::new(&winit_state.window) {
    Ok(state) => state,
    Err(e) => panic!(e),
  };

  let mut running = true;
  let (mut frame_width, mut frame_height) = winit_state
    .window
    .get_inner_size()
    .map(|logical| logical.into())
    .unwrap_or((0.0, 0.0));
  let (mut mouse_x, mut mouse_y) = (0.0, 0.0);

  'main_loop: loop {
    winit_state.events_loop.poll_events(|event| match event {
      Event::WindowEvent {
        event: WindowEvent::CloseRequested,
        ..
      } => running = false,
      Event::WindowEvent {
        event: WindowEvent::Resized(logical),
        ..
      } => {
        frame_width = logical.width;
        frame_height = logical.height;
      }
      Event::WindowEvent {
        event: WindowEvent::CursorMoved { position, .. },
        ..
      } => {
        mouse_x = position.x;
        mouse_y = position.y;
      }
      _ => (),
    });
    if !running {
      break 'main_loop;
    }

    // This makes a color that changes as the mouse moves, just so that there's
    // some feedback that we're really drawing a new thing each frame.
    let r = (mouse_x / frame_width) as f32;
    let g = (mouse_y / frame_height) as f32;
    let b = (r + g) * 0.3;
    let a = 1.0;

    if let Err(e) = hal_state.draw_clear_frame([r, g, b, a]) {
      error!("Error while drawing a clear frame: {}", e);
      break 'main_loop;
    }
  }

  // If we leave the main loop for any reason, we want to shut down as
  // gracefully as we can.
  if let Err(e) = hal_state.wait_until_idle() {
    error!("Error while waiting for the queues to idle: {}", e);
  }
}