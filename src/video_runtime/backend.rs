use crate::{
    message::Message,
    state::VideoClip,
    video_runtime::types::{VideoFrameLoadState, VideoFrameRef, VideoRuntimeBackend},
};
use iced::Task;
use maolan_engine::message::VideoClipData;
use std::collections::HashSet;

pub trait VideoBackend {
    #[allow(dead_code)]
    fn kind(&self) -> VideoRuntimeBackend;

    fn preview_frame<'a>(&self, clip: &'a VideoClip) -> Option<VideoFrameRef<'a>>;

    fn current_frame<'a>(&self, clip: &'a VideoClip) -> Option<VideoFrameRef<'a>>;

    fn preview_load_state(&self, clip: &VideoClip) -> Option<VideoFrameLoadState>;

    fn current_load_state(&self, clip: &VideoClip) -> Option<VideoFrameLoadState>;

    fn request_preview_frame(
        &self,
        track_name: String,
        clip: VideoClipData,
        sample_rate: f64,
    ) -> Task<Message>;

    fn request_current_frame(
        &self,
        track_name: String,
        clip: VideoClipData,
        sample_rate: f64,
        sample: usize,
    ) -> Task<Message>;

    fn finish_decode(&self, clip_key: String, preview: bool, generation: u64) -> Task<Message>;

    fn retain_clip_keys(&self, clip_keys: &HashSet<String>);
}
