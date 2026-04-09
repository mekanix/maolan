use maolan_engine::{message::VideoFrameBuffer, mutex::UnsafeMutex};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VideoRuntimeBackendPreference {
    Cpu,
    #[default]
    Auto,
    Vulkan,
}

impl VideoRuntimeBackendPreference {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Cpu, Self::Vulkan];
}

impl fmt::Display for VideoRuntimeBackendPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => write!(f, "Auto"),
            Self::Cpu => write!(f, "CPU"),
            Self::Vulkan => write!(f, "Vulkan"),
        }
    }
}

impl FromStr for VideoRuntimeBackendPreference {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "cpu" => Ok(Self::Cpu),
            "vulkan" => Ok(Self::Vulkan),
            _ => Err(()),
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoFrameLoadState {
    Loading,
    Failed(String),
}

#[derive(Clone)]
pub enum VideoFrameRef<'a> {
    Cpu(&'a Arc<UnsafeMutex<VideoFrameBuffer>>),
    Gpu {
        handle: VideoTextureHandle,
        metadata: VideoFrameMetadata,
    },
}
