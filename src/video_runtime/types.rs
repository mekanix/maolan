use maolan_engine::{message::VideoFrameBuffer, mutex::UnsafeMutex};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoRuntimeBackend {
    Cpu,
    Vulkan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoTextureHandle {
    pub slot: u32,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrameMetadata {
    pub width: u32,
    pub height: u32,
    pub pts_samples: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrameDebugInfo {
    pub producer_label: String,
    pub fallback_reason: Option<String>,
}

#[derive(Clone)]
pub enum VideoFrameRef<'a> {
    Cpu(&'a Arc<UnsafeMutex<VideoFrameBuffer>>),
    Gpu {
        handle: VideoTextureHandle,
        metadata: VideoFrameMetadata,
    },
}
