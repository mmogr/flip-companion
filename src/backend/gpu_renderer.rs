//! GPU rendering backend using GBM + EGL + GLES via smithay.
//!
//! Provides GBM scanout buffer allocation, EGL/GLES context initialization,
//! and double-buffered rendering with DRM page-flip. Designed for the
//! AYANEO Flip DS bottom panel (1080×1620 portrait, 60 Hz).
//!
//! # Usage
//!
//! ```ignore
//! let mut gpu = GpuRenderer::new(lease_fd)?;
//! gpu.render_frame(Transform::_270, |frame| {
//!     // Draw landscape content — the transform rotates 90° CW
//!     frame.clear([0.0, 0.0, 0.0, 1.0], &[])?;
//! })?;
//! ```

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use drm::control::Device as ControlDevice;
use drm_fourcc::DrmModifier;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::dmabuf::AsDmabuf;
use smithay::backend::allocator::{Allocator, Fourcc};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer};
use smithay::backend::renderer::{Bind, Frame, ImportDma, Renderer};
use smithay::utils::{Physical, Size, Transform};

// ── Error type ──────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum GpuError {
    Io(std::io::Error),
    Egl(smithay::backend::egl::Error),
    Gles(smithay::backend::renderer::gles::GlesError),
    Gbm(String),
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO: {e}"),
            Self::Egl(e) => write!(f, "EGL: {e}"),
            Self::Gles(e) => write!(f, "GLES: {e}"),
            Self::Gbm(e) => write!(f, "GBM: {e}"),
        }
    }
}

impl std::error::Error for GpuError {}

impl From<std::io::Error> for GpuError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<smithay::backend::egl::Error> for GpuError {
    fn from(e: smithay::backend::egl::Error) -> Self {
        Self::Egl(e)
    }
}
impl From<smithay::backend::renderer::gles::GlesError> for GpuError {
    fn from(e: smithay::backend::renderer::gles::GlesError) -> Self {
        Self::Gles(e)
    }
}

// ── DRM fd wrapper ──────────────────────────────────────────────────────

struct DrmCard(OwnedFd);

impl AsFd for DrmCard {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl AsRawFd for DrmCard {
    fn as_raw_fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
}
impl drm::Device for DrmCard {}
impl drm::control::Device for DrmCard {}

// ── Scanout buffer ──────────────────────────────────────────────────────

struct ScanoutBuffer {
    _bo: smithay::backend::allocator::gbm::GbmBuffer,
    fb: drm::control::framebuffer::Handle,
    dmabuf: Dmabuf,
}

// ── GPU renderer ────────────────────────────────────────────────────────

/// GPU-accelerated renderer with double-buffered GBM scanout.
///
/// Owns the EGL context and GLES renderer. All GL operations must happen
/// on the thread that owns this struct (`GlesRenderer` is `!Send + !Sync`).
pub struct GpuRenderer {
    drm: DrmCard,
    _display: EGLDisplay,
    renderer: GlesRenderer,
    bufs: [ScanoutBuffer; 2],
    front: usize,
    connector: drm::control::connector::Handle,
    crtc: drm::control::crtc::Handle,
    mode: drm::control::Mode,
    output_size: Size<i32, Physical>,
}

impl GpuRenderer {
    /// Create a new GPU renderer from a DRM lease fd.
    ///
    /// Probes the leased connector/CRTC, creates GBM + EGL + GLES,
    /// allocates two XRGB8888 scanout buffers, and performs the initial
    /// mode-set.
    pub fn new(lease_fd: OwnedFd) -> Result<Self, GpuError> {
        // Dup the fd: one for DRM ioctls, one for GBM buffer allocation,
        // one consumed by EGLDisplay. All dup'd fds share the same DRM
        // open file description, so GEM handles are valid across them.
        let drm_fd = lease_fd.as_fd().try_clone_to_owned()?;
        let gbm_fd = lease_fd.as_fd().try_clone_to_owned()?;
        let egl_fd = lease_fd;

        let drm = DrmCard(drm_fd);

        // ── Probe DRM resources ─────────────────────────────────────
        let res = drm.resource_handles()?;
        if res.connectors().is_empty() || res.crtcs().is_empty() {
            return Err(GpuError::Gbm(
                "lease has no connectors or CRTCs".into(),
            ));
        }
        let connector = res.connectors()[0];
        let crtc = res.crtcs()[0];

        let conn_info = drm.get_connector(connector, false)?;
        if conn_info.modes().is_empty() {
            return Err(GpuError::Gbm("connector has no modes".into()));
        }
        let mode = conn_info.modes()[0];
        let phys_w = mode.size().0 as u32; // 1080
        let phys_h = mode.size().1 as u32; // 1620

        eprintln!(
            "[gpu] physical: {}x{}@{}Hz",
            phys_w,
            phys_h,
            mode.vrefresh()
        );

        // ── GBM allocator for scanout buffers ───────────────────────
        let gbm_device = GbmDevice::new(gbm_fd)
            .map_err(|e| GpuError::Gbm(format!("GbmDevice::new: {e}")))?;
        let mut allocator = GbmAllocator::new(
            gbm_device,
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );

        // ── EGL display + context from a separate GBM device ────────
        let egl_gbm = GbmDevice::new(egl_fd)
            .map_err(|e| GpuError::Gbm(format!("GbmDevice (egl): {e}")))?;

        // SAFETY: We own the GBM device backing this EGLDisplay and do
        // not call eglGetPlatformDisplay from any other code path.
        let display = unsafe { EGLDisplay::new(egl_gbm)? };

        eprintln!(
            "[gpu] EGL {}.{}",
            display.get_egl_version().0,
            display.get_egl_version().1
        );

        let context = EGLContext::new(&display)?;

        // SAFETY: This EGLContext is only ever current on this thread.
        let renderer = unsafe { GlesRenderer::new(context)? };

        // ── Allocate double scanout buffers ──────────────────────────
        let buf0 =
            Self::alloc_scanout(&mut allocator, &drm, phys_w, phys_h)?;
        let buf1 =
            Self::alloc_scanout(&mut allocator, &drm, phys_w, phys_h)?;

        // ── Initial mode-set ────────────────────────────────────────
        drm.set_crtc(
            crtc,
            Some(buf0.fb),
            (0, 0),
            &[connector],
            Some(mode),
        )?;

        let output_size = Size::from((phys_w as i32, phys_h as i32));

        eprintln!(
            "[gpu] ready: double-buffered {}x{} XRGB8888",
            phys_w, phys_h
        );

        Ok(Self {
            drm,
            _display: display,
            renderer,
            bufs: [buf0, buf1],
            front: 0,
            connector,
            crtc,
            mode,
            output_size,
        })
    }

    /// Allocate a single GBM scanout buffer and create a DRM framebuffer.
    fn alloc_scanout(
        allocator: &mut GbmAllocator<OwnedFd>,
        drm: &DrmCard,
        width: u32,
        height: u32,
    ) -> Result<ScanoutBuffer, GpuError> {
        let buffer = allocator
            .create_buffer(width, height, Fourcc::Xrgb8888, &[DrmModifier::Linear])
            .map_err(|e| GpuError::Gbm(format!("create_buffer: {e}")))?;

        // Export as dmabuf for GL rendering via Bind<Dmabuf>
        let dmabuf = buffer
            .export()
            .map_err(|e| GpuError::Gbm(format!("export dmabuf: {e}")))?;

        // Create DRM framebuffer from the GBM buffer object.
        // GbmBuffer derefs to gbm::BufferObject which implements drm::buffer::Buffer.
        let fb = drm.add_framebuffer(&buffer, 24, 32)?;

        eprintln!("[gpu] scanout buffer {}x{} fb={:?}", width, height, fb);

        Ok(ScanoutBuffer {
            _bo: buffer,
            fb,
            dmabuf,
        })
    }

    /// Access the underlying GLES renderer for texture operations
    /// (e.g., `import_memory`, `import_dmabuf`).
    pub fn renderer(&mut self) -> &mut GlesRenderer {
        &mut self.renderer
    }

    /// Physical output size in pixels (portrait: 1080×1620).
    pub fn output_size(&self) -> Size<i32, Physical> {
        self.output_size
    }

    /// DRM connector handle.
    pub fn connector(&self) -> drm::control::connector::Handle {
        self.connector
    }

    /// DRM CRTC handle.
    pub fn crtc(&self) -> drm::control::crtc::Handle {
        self.crtc
    }

    /// Supported DMA-BUF format+modifier pairs for texture import (from EGL).
    pub fn dmabuf_formats(&self) -> FormatSet {
        self.renderer.dmabuf_formats()
    }

    /// Render a frame to the back buffer, then page-flip.
    ///
    /// The `transform` controls output rotation:
    /// - `Transform::Normal` — no rotation (portrait content)
    /// - `Transform::_270` — 90° CW rotation (landscape → portrait)
    ///
    /// The callback draws into the `GlesFrame`. After it returns, the
    /// frame is finished, the GPU sync is waited on, and the buffer is
    /// page-flipped to the display via `set_crtc`.
    pub fn render_frame(
        &mut self,
        transform: Transform,
        render_fn: impl FnOnce(&mut GlesFrame<'_, '_>),
    ) -> Result<(), GpuError> {
        let back = 1 - self.front;

        // Bind the back buffer's dmabuf as the render target
        let dmabuf = &mut self.bufs[back].dmabuf;
        let mut target = self.renderer.bind(dmabuf)?;

        // Begin a render pass
        let mut frame =
            self.renderer
                .render(&mut target, self.output_size, transform)?;

        // Let the caller draw
        render_fn(&mut frame);

        // Finish the frame and wait for GPU completion
        let sync = frame.finish()?;
        self.renderer.wait(&sync)?;
        drop(target);

        // Page flip: display the back buffer
        self.drm.set_crtc(
            self.crtc,
            Some(self.bufs[back].fb),
            (0, 0),
            &[self.connector],
            Some(self.mode),
        )?;

        self.front = back;
        Ok(())
    }
}
