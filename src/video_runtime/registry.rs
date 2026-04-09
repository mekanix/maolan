use crate::video_runtime::types::{VideoFrameMetadata, VideoTextureHandle};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum RegisteredVideoTextureSource {
    Rgba8 {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
pub struct RegisteredVideoTexture {
    pub metadata: VideoFrameMetadata,
    pub source: RegisteredVideoTextureSource,
    pub producer_label: String,
    pub fallback_reason: Option<String>,
    pub revision: u64,
}

#[derive(Debug, Default)]
pub struct VideoTextureRegistry {
    next_slot: u32,
    generation: u64,
    next_revision: u64,
    textures: HashMap<VideoTextureHandle, RegisteredVideoTexture>,
}

impl VideoTextureRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        metadata: VideoFrameMetadata,
        source: RegisteredVideoTextureSource,
        producer_label: String,
        fallback_reason: Option<String>,
    ) -> VideoTextureHandle {
        let revision = self.bump_revision();
        let handle = VideoTextureHandle {
            slot: self.next_slot,
            generation: self.generation,
        };
        self.next_slot = self.next_slot.wrapping_add(1);
        self.textures.insert(
            handle,
            RegisteredVideoTexture {
                metadata,
                source,
                producer_label,
                fallback_reason,
                revision,
            },
        );
        handle
    }

    pub fn replace(
        &mut self,
        handle: VideoTextureHandle,
        metadata: VideoFrameMetadata,
        source: RegisteredVideoTextureSource,
        producer_label: String,
        fallback_reason: Option<String>,
    ) -> Option<()> {
        let revision = self.bump_revision();
        let registered = self.textures.get_mut(&handle)?;
        registered.metadata = metadata;
        registered.source = source;
        registered.producer_label = producer_label;
        registered.fallback_reason = fallback_reason;
        registered.revision = revision;
        Some(())
    }

    pub fn get(&self, handle: &VideoTextureHandle) -> Option<&RegisteredVideoTexture> {
        self.textures.get(handle)
    }

    pub fn remove(&mut self, handle: &VideoTextureHandle) -> Option<RegisteredVideoTexture> {
        self.textures.remove(handle)
    }

    pub fn clear(&mut self) {
        self.textures.clear();
        self.generation = self.generation.wrapping_add(1);
        self.next_slot = 0;
    }

    fn bump_revision(&mut self) -> u64 {
        let revision = self.next_revision;
        self.next_revision = self.next_revision.wrapping_add(1);
        revision
    }
}
