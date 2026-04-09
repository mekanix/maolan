pub(crate) mod backend;
pub(crate) mod cpu;
pub(crate) mod presenter;
pub(crate) mod registry;
pub(crate) mod types;
pub(crate) mod vulkan;
pub(crate) mod widget;

use crate::{message::Message, state::VideoClip};
use iced::Task;
use maolan_engine::message::VideoClipData;
use presenter::{PresentableFrame, VideoPresenter};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use types::{VideoFrameDebugInfo, VideoFrameRef, VideoRuntimeBackend};

use self::{
    backend::VideoBackend,
    cpu::CpuBackend,
    registry::VideoTextureRegistry,
    vulkan::{VulkanBackend, VulkanDeviceContext, VulkanFrameProducerKind, VulkanRuntimeConfig},
};

pub struct VideoRuntime {
    active_backend: VideoRuntimeBackend,
    cpu_backend: CpuBackend,
    vulkan_backend: VulkanBackend,
    presenter: VideoPresenter,
}

impl VideoRuntime {
    pub fn new() -> Self {
        let active_backend = Self::default_backend_from_env();
        Self {
            active_backend,
            cpu_backend: CpuBackend::new(),
            vulkan_backend: Self::default_vulkan_backend(),
            presenter: VideoPresenter::new(),
        }
    }

    fn default_backend_from_env() -> VideoRuntimeBackend {
        match std::env::var("MAOLAN_VIDEO_BACKEND")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("vulkan") => VideoRuntimeBackend::Vulkan,
            _ => VideoRuntimeBackend::Cpu,
        }
    }

    fn default_vulkan_backend() -> VulkanBackend {
        VulkanBackend::with_producer_kind(
            VulkanRuntimeConfig {
                device: VulkanDeviceContext {
                    device_label: "iced-wgpu".to_string(),
                    queue_family_index: 0,
                },
                decode_queue_family_index: 0,
                presentation_queue_family_index: 0,
            },
            Self::default_vulkan_producer_kind_from_env(),
        )
    }

    fn default_vulkan_producer_kind_from_env() -> VulkanFrameProducerKind {
        match std::env::var("MAOLAN_VIDEO_VULKAN_PRODUCER")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("hardware") => VulkanFrameProducerKind::Hardware,
            Some("cpu-upload") => VulkanFrameProducerKind::CpuUpload,
            _ => VulkanFrameProducerKind::Auto,
        }
    }

    #[allow(dead_code)]
    pub fn configure_vulkan_backend(
        &mut self,
        config: VulkanRuntimeConfig,
        producer_kind: VulkanFrameProducerKind,
    ) {
        self.vulkan_backend = VulkanBackend::with_producer_kind(config, producer_kind);
        if self.active_backend == VideoRuntimeBackend::Vulkan {
            self.active_backend = VideoRuntimeBackend::Vulkan;
        }
    }

    #[allow(dead_code)]
    pub fn backend(&self) -> Option<VideoRuntimeBackend> {
        Some(self.active_backend)
    }

    #[allow(dead_code)]
    pub fn set_backend(&mut self, backend: VideoRuntimeBackend) {
        self.active_backend = backend;
    }

    pub fn texture_registry(&self) -> Option<Arc<Mutex<VideoTextureRegistry>>> {
        match self.active_backend {
            VideoRuntimeBackend::Cpu => None,
            VideoRuntimeBackend::Vulkan => Some(self.vulkan_backend.texture_registry()),
        }
    }

    #[allow(dead_code)]
    pub fn frame_debug_info(&self, frame: &VideoFrameRef<'_>) -> Option<VideoFrameDebugInfo> {
        match frame {
            VideoFrameRef::Cpu(_) => Some(VideoFrameDebugInfo {
                producer_label: "cpu".to_string(),
                fallback_reason: None,
            }),
            VideoFrameRef::Gpu { handle, .. } => {
                let registry = self.texture_registry()?;
                let registry = registry.lock().ok()?;
                let registered = registry.get(handle)?;
                Some(VideoFrameDebugInfo {
                    producer_label: registered.producer_label.clone(),
                    fallback_reason: registered.fallback_reason.clone(),
                })
            }
        }
    }

    fn backend_impl(&self) -> &dyn VideoBackend {
        match self.active_backend {
            VideoRuntimeBackend::Cpu => &self.cpu_backend,
            VideoRuntimeBackend::Vulkan => &self.vulkan_backend,
        }
    }

    pub fn preview_frame<'a>(&self, clip: &'a VideoClip) -> Option<VideoFrameRef<'a>> {
        self.backend_impl().preview_frame(clip)
    }

    pub fn current_frame<'a>(&self, clip: &'a VideoClip) -> Option<VideoFrameRef<'a>> {
        self.backend_impl().current_frame(clip)
    }

    #[allow(dead_code)]
    pub fn image_handle(&self, frame: &VideoFrameRef<'_>) -> Option<iced::widget::image::Handle> {
        self.presenter.image_handle(frame)
    }

    pub fn presentable_frame(&self, frame: &VideoFrameRef<'_>) -> Option<PresentableFrame> {
        self.presenter.presentable_frame(frame)
    }

    pub fn request_preview_frame(
        &self,
        track_name: String,
        clip: VideoClipData,
        sample_rate: f64,
    ) -> Task<Message> {
        self.backend_impl()
            .request_preview_frame(track_name, clip, sample_rate)
    }

    pub fn request_current_frame(
        &self,
        track_name: String,
        clip: VideoClipData,
        sample_rate: f64,
        sample: usize,
    ) -> Task<Message> {
        self.backend_impl()
            .request_current_frame(track_name, clip, sample_rate, sample)
    }

    pub fn finish_decode(&self, clip_key: String, preview: bool, generation: u64) -> Task<Message> {
        self.backend_impl()
            .finish_decode(clip_key, preview, generation)
    }

    pub fn retain_clip_keys(&self, clip_keys: &HashSet<String>) {
        self.backend_impl().retain_clip_keys(clip_keys);
    }
}

impl Default for VideoRuntime {
    fn default() -> Self {
        Self::new()
    }
}
