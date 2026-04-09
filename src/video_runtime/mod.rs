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
use types::{
    VideoFrameDebugInfo, VideoFrameLoadState, VideoFrameRef, VideoRuntimeBackend,
    VideoRuntimeBackendPreference,
};

use self::{
    backend::VideoBackend,
    cpu::CpuBackend,
    registry::VideoTextureRegistry,
    vulkan::{VulkanBackend, VulkanDeviceContext, VulkanFrameProducerKind, VulkanRuntimeConfig},
};

pub struct VideoRuntime {
    backend_preference: VideoRuntimeBackendPreference,
    active_backend: VideoRuntimeBackend,
    cpu_backend: CpuBackend,
    vulkan_backend: VulkanBackend,
    presenter: VideoPresenter,
}

impl VideoRuntime {
    pub fn new() -> Self {
        Self::new_with_preferences(
            Self::backend_preference_from_env().unwrap_or_default(),
            Self::default_vulkan_producer_kind_from_env(),
        )
    }

    pub fn new_with_preferences(
        preference: VideoRuntimeBackendPreference,
        producer_kind: VulkanFrameProducerKind,
    ) -> Self {
        let vulkan_backend = Self::default_vulkan_backend(producer_kind);
        let active_backend =
            Self::resolve_backend_preference(preference, vulkan_backend.is_available());
        Self {
            backend_preference: preference,
            active_backend,
            cpu_backend: CpuBackend::new(),
            vulkan_backend,
            presenter: VideoPresenter::new(),
        }
    }

    fn backend_preference_from_env() -> Option<VideoRuntimeBackendPreference> {
        std::env::var("MAOLAN_VIDEO_BACKEND")
            .ok()
            .and_then(|value| value.parse().ok())
    }

    fn resolve_backend_preference(
        preference: VideoRuntimeBackendPreference,
        vulkan_available: bool,
    ) -> VideoRuntimeBackend {
        match preference {
            VideoRuntimeBackendPreference::Auto if vulkan_available => VideoRuntimeBackend::Vulkan,
            VideoRuntimeBackendPreference::Vulkan if vulkan_available => {
                VideoRuntimeBackend::Vulkan
            }
            _ => VideoRuntimeBackend::Cpu,
        }
    }

    fn default_vulkan_backend(producer_kind: VulkanFrameProducerKind) -> VulkanBackend {
        VulkanBackend::with_producer_kind(
            VulkanRuntimeConfig {
                device: VulkanDeviceContext {
                    device_label: "iced-wgpu".to_string(),
                    queue_family_index: 0,
                },
                decode_queue_family_index: 0,
                presentation_queue_family_index: 0,
            },
            producer_kind,
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
        self.active_backend = Self::resolve_backend_preference(
            self.backend_preference,
            self.vulkan_backend.is_available(),
        );
    }

    pub fn backend(&self) -> VideoRuntimeBackend {
        self.backend_impl().kind()
    }

    #[allow(dead_code)]
    pub fn set_backend(&mut self, backend: VideoRuntimeBackend) {
        self.active_backend = match backend {
            VideoRuntimeBackend::Cpu => VideoRuntimeBackend::Cpu,
            VideoRuntimeBackend::Vulkan if self.vulkan_backend.is_available() => {
                VideoRuntimeBackend::Vulkan
            }
            VideoRuntimeBackend::Vulkan => VideoRuntimeBackend::Cpu,
        };
    }

    pub fn backend_preference(&self) -> VideoRuntimeBackendPreference {
        self.backend_preference
    }

    #[allow(dead_code)]
    pub fn set_backend_preference(&mut self, preference: VideoRuntimeBackendPreference) {
        self.backend_preference = preference;
        self.active_backend =
            Self::resolve_backend_preference(preference, self.vulkan_backend.is_available());
    }

    pub fn vulkan_producer_kind(&self) -> VulkanFrameProducerKind {
        self.vulkan_backend.producer_kind()
    }

    pub fn set_preferences(
        &mut self,
        backend_preference: VideoRuntimeBackendPreference,
        producer_kind: VulkanFrameProducerKind,
    ) {
        self.backend_preference = backend_preference;
        self.vulkan_backend = Self::default_vulkan_backend(producer_kind);
        self.active_backend = Self::resolve_backend_preference(
            backend_preference,
            self.vulkan_backend.is_available(),
        );
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

    pub fn preview_load_state(&self, clip: &VideoClip) -> Option<VideoFrameLoadState> {
        self.backend_impl().preview_load_state(clip)
    }

    pub fn current_load_state(&self, clip: &VideoClip) -> Option<VideoFrameLoadState> {
        self.backend_impl().current_load_state(clip)
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_backend_preference_defaults_to_cpu() {
        assert_eq!(
            VideoRuntime::resolve_backend_preference(VideoRuntimeBackendPreference::Cpu, true),
            VideoRuntimeBackend::Cpu
        );
    }

    #[test]
    fn resolve_backend_preference_uses_auto_when_vulkan_available() {
        assert_eq!(
            VideoRuntime::resolve_backend_preference(VideoRuntimeBackendPreference::Auto, true),
            VideoRuntimeBackend::Vulkan
        );
    }

    #[test]
    fn resolve_backend_preference_auto_falls_back_to_cpu_when_unavailable() {
        assert_eq!(
            VideoRuntime::resolve_backend_preference(VideoRuntimeBackendPreference::Auto, false),
            VideoRuntimeBackend::Cpu
        );
    }

    #[test]
    fn resolve_backend_preference_vulkan_falls_back_to_cpu_when_unavailable() {
        assert_eq!(
            VideoRuntime::resolve_backend_preference(VideoRuntimeBackendPreference::Vulkan, false),
            VideoRuntimeBackend::Cpu
        );
    }

    #[test]
    fn backend_preference_from_env_parses_known_values() {
        assert_eq!(
            "auto".parse::<VideoRuntimeBackendPreference>().ok(),
            Some(VideoRuntimeBackendPreference::Auto)
        );
        assert_eq!(
            "cpu".parse::<VideoRuntimeBackendPreference>().ok(),
            Some(VideoRuntimeBackendPreference::Cpu)
        );
        assert_eq!(
            "vulkan".parse::<VideoRuntimeBackendPreference>().ok(),
            Some(VideoRuntimeBackendPreference::Vulkan)
        );
    }
}
