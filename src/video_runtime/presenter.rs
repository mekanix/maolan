use crate::video_runtime::types::{VideoFrameRef, VideoTextureHandle};
use iced::widget::image;

pub enum PresentableFrame {
    CpuImage(image::Handle),
    GpuTexture(VideoTextureHandle),
}

pub struct VideoPresenter;

impl VideoPresenter {
    pub fn new() -> Self {
        Self
    }

    pub fn presentable_frame(&self, frame: &VideoFrameRef<'_>) -> Option<PresentableFrame> {
        match frame {
            VideoFrameRef::Cpu(frame) => {
                let frame = frame.lock();
                if frame.width > 0 && frame.height > 0 && !frame.rgba.is_empty() {
                    Some(PresentableFrame::CpuImage(image::Handle::from_rgba(
                        frame.width,
                        frame.height,
                        frame.rgba.clone(),
                    )))
                } else {
                    None
                }
            }
            VideoFrameRef::Gpu { handle, .. } => Some(PresentableFrame::GpuTexture(*handle)),
        }
    }

    #[allow(dead_code)]
    pub fn image_handle(&self, frame: &VideoFrameRef<'_>) -> Option<image::Handle> {
        match self.presentable_frame(frame) {
            Some(PresentableFrame::CpuImage(handle)) => Some(handle),
            Some(PresentableFrame::GpuTexture(_)) | None => None,
        }
    }
}

impl Default for VideoPresenter {
    fn default() -> Self {
        Self::new()
    }
}
