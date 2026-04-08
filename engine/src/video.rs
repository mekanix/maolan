use crate::{
    message::{VideoClipData, VideoFrameBuffer},
    mutex::UnsafeMutex,
};
use ffmpeg_next::{
    Rational, codec, format, frame, media,
    software::scaling::{context::Context as ScalingContext, flag::Flags},
    util::format::pixel::Pixel,
};
use std::sync::{Arc, OnceLock};

const PREVIEW_THUMB_HEIGHT: u32 = 48;
const PREVIEW_MAX_THUMBS: usize = 8;
const FRAME_SEEK_PREROLL_US: i64 = 500_000;

fn ffmpeg_init() -> Result<(), ffmpeg_next::Error> {
    static RESULT: OnceLock<Result<(), ffmpeg_next::Error>> = OnceLock::new();
    *RESULT.get_or_init(ffmpeg_next::init)
}

fn rgba_pixels(frame: &frame::Video, width: u32, height: u32) -> Vec<u8> {
    let stride = frame.stride(0);
    let row_len = (width as usize) * 4;
    let data = frame.data(0);
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height as usize {
        let start = y * stride;
        let end = start + row_len;
        pixels.extend_from_slice(&data[start..end]);
    }
    pixels
}

pub struct VideoDecoderState {
    clip: VideoClipData,
    sample_rate: f64,
    input: format::context::Input,
    stream_index: usize,
    time_base: Rational,
    decoder: codec::decoder::Video,
    scaler: ScalingContext,
    decoded: frame::Video,
    rgba: frame::Video,
    current_seconds: Option<f64>,
    last_frame: Option<VideoFrameBuffer>,
}

unsafe impl Send for VideoDecoderState {}
unsafe impl Sync for VideoDecoderState {}

impl std::fmt::Debug for VideoDecoderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoDecoderState")
            .field("clip", &self.clip)
            .field("sample_rate", &self.sample_rate)
            .field("stream_index", &self.stream_index)
            .field("current_seconds", &self.current_seconds)
            .field(
                "last_frame",
                &self.last_frame.as_ref().map(|frame| {
                    (
                        frame.width,
                        frame.height,
                        frame.pts_samples,
                        frame.rgba.len(),
                    )
                }),
            )
            .finish()
    }
}

impl VideoDecoderState {
    pub fn new(clip: VideoClipData, sample_rate: f64) -> Result<Self, String> {
        ffmpeg_init().map_err(|e| format!("ffmpeg init failed: {e}"))?;

        let input = format::input(&clip.path).map_err(|e| format!("open video failed: {e}"))?;
        let Some(stream) = input.streams().best(media::Type::Video) else {
            return Err("no video stream found".to_string());
        };
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let context = codec::Context::from_parameters(stream.parameters())
            .map_err(|e| format!("video codec params failed: {e}"))?;
        let decoder = context
            .decoder()
            .video()
            .map_err(|e| format!("video decoder open failed: {e}"))?;

        let width = decoder.width().max(1);
        let height = decoder.height().max(1);
        let scaler = ScalingContext::get(
            decoder.format(),
            width,
            height,
            Pixel::RGBA,
            width,
            height,
            Flags::BILINEAR,
        )
        .map_err(|e| format!("video scaler init failed: {e}"))?;

        Ok(Self {
            clip,
            sample_rate,
            input,
            stream_index,
            time_base,
            decoder,
            scaler,
            decoded: frame::Video::empty(),
            rgba: frame::Video::empty(),
            current_seconds: None,
            last_frame: None,
        })
    }

    fn maybe_seek(&mut self, target_seconds: f64) -> Result<(), String> {
        let should_seek = self.current_seconds.is_some_and(|current| {
            target_seconds + 1.0e-6 < current || target_seconds - current > 1.0
        });
        if !should_seek {
            return Ok(());
        }

        let target_us = (target_seconds * 1_000_000.0).max(0.0) as i64;
        let seek_us = target_us.saturating_sub(FRAME_SEEK_PREROLL_US);
        self.input
            .seek(seek_us, ..)
            .map_err(|e| format!("video seek failed: {e}"))?;
        self.decoder.flush();
        self.current_seconds = None;
        self.last_frame = None;
        Ok(())
    }

    pub fn decode_frame_at_sample(
        &mut self,
        target_sample: usize,
    ) -> Result<Arc<UnsafeMutex<VideoFrameBuffer>>, String> {
        let clip_local_sample = target_sample
            .saturating_sub(self.clip.start)
            .saturating_add(self.clip.offset)
            .min(self.clip.offset.saturating_add(self.clip.length));
        let target_seconds = clip_local_sample as f64 / self.sample_rate.max(1.0);

        self.maybe_seek(target_seconds)?;

        if let Some(frame) = &self.last_frame {
            let last_seconds = frame.pts_samples as f64 / self.sample_rate.max(1.0);
            if last_seconds >= target_seconds {
                return Ok(Arc::new(UnsafeMutex::new(frame.clone())));
            }
        }

        let mut fallback = self.last_frame.clone();
        for (packet_stream, packet) in self.input.packets() {
            if packet_stream.index() != self.stream_index {
                continue;
            }
            self.decoder
                .send_packet(&packet)
                .map_err(|e| format!("send video packet failed: {e}"))?;
            while self.decoder.receive_frame(&mut self.decoded).is_ok() {
                self.scaler
                    .run(&self.decoded, &mut self.rgba)
                    .map_err(|e| format!("convert video frame failed: {e}"))?;

                let pts = self.decoded.timestamp().unwrap_or_default();
                let frame_seconds = (pts as f64) * f64::from(self.time_base);
                let frame_buffer = VideoFrameBuffer {
                    width: self.decoder.width().max(1),
                    height: self.decoder.height().max(1),
                    rgba: rgba_pixels(
                        &self.rgba,
                        self.decoder.width().max(1),
                        self.decoder.height().max(1),
                    ),
                    pts_samples: (frame_seconds * self.sample_rate.max(1.0)).max(0.0) as usize,
                };

                self.current_seconds = Some(frame_seconds);
                self.last_frame = Some(frame_buffer.clone());
                fallback = Some(frame_buffer.clone());
                if frame_seconds + 1.0e-6 >= target_seconds {
                    return Ok(Arc::new(UnsafeMutex::new(frame_buffer)));
                }
            }
        }

        fallback
            .map(|frame| Arc::new(UnsafeMutex::new(frame)))
            .ok_or_else(|| "no decoded video frame available".to_string())
    }
}

pub fn decode_iframe_preview_strip(
    clip: &VideoClipData,
    _sample_rate: f64,
) -> Result<Arc<UnsafeMutex<VideoFrameBuffer>>, String> {
    ffmpeg_init().map_err(|e| format!("ffmpeg init failed: {e}"))?;

    let mut input = format::input(&clip.path).map_err(|e| format!("open video failed: {e}"))?;
    let Some(stream) = input.streams().best(media::Type::Video) else {
        return Err("no video stream found".to_string());
    };
    let stream_index = stream.index();
    let context = codec::Context::from_parameters(stream.parameters())
        .map_err(|e| format!("video codec params failed: {e}"))?;
    let mut decoder = context
        .decoder()
        .video()
        .map_err(|e| format!("video decoder open failed: {e}"))?;

    let src_format = decoder.format();
    let width = decoder.width().max(1);
    let height = decoder.height().max(1);
    let thumb_height = PREVIEW_THUMB_HEIGHT.min(height).max(1);
    let thumb_width =
        (((width as f64) * (thumb_height as f64) / (height as f64)).round() as u32).max(1);
    let mut scaler = ScalingContext::get(
        src_format,
        width,
        height,
        Pixel::RGBA,
        thumb_width,
        thumb_height,
        Flags::BILINEAR,
    )
    .map_err(|e| format!("video scaler init failed: {e}"))?;

    let mut decoded = frame::Video::empty();
    let mut rgba = frame::Video::empty();
    let mut keyframes = Vec::<Vec<u8>>::new();
    let mut fallback = None::<Vec<u8>>;

    for (packet_stream, packet) in input.packets() {
        if packet_stream.index() != stream_index {
            continue;
        }
        decoder
            .send_packet(&packet)
            .map_err(|e| format!("send video packet failed: {e}"))?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            scaler
                .run(&decoded, &mut rgba)
                .map_err(|e| format!("convert video frame failed: {e}"))?;
            let pixels = rgba_pixels(&rgba, thumb_width, thumb_height);
            if fallback.is_none() {
                fallback = Some(pixels.clone());
            }
            if decoded.is_key() {
                keyframes.push(pixels);
                if keyframes.len() >= PREVIEW_MAX_THUMBS.saturating_mul(4) {
                    break;
                }
            }
        }
        if keyframes.len() >= PREVIEW_MAX_THUMBS.saturating_mul(4) {
            break;
        }
    }

    if keyframes.is_empty()
        && let Some(frame) = fallback
    {
        keyframes.push(frame);
    }
    if keyframes.is_empty() {
        return Err("no decoded video frame available".to_string());
    }

    let desired = keyframes.len().clamp(1, PREVIEW_MAX_THUMBS);
    let mut selected = Vec::with_capacity(desired);
    for index in 0..desired {
        let src_index = index * keyframes.len() / desired;
        selected.push(keyframes[src_index].clone());
    }

    let strip_width = thumb_width.saturating_mul(selected.len() as u32).max(1);
    let mut strip = vec![0_u8; (strip_width as usize) * (thumb_height as usize) * 4];
    let row_bytes = (thumb_width as usize) * 4;
    let strip_row_bytes = (strip_width as usize) * 4;
    for (thumb_index, thumb) in selected.iter().enumerate() {
        let x_offset = thumb_index * row_bytes;
        for row in 0..thumb_height as usize {
            let src_start = row * row_bytes;
            let src_end = src_start + row_bytes;
            let dst_start = row * strip_row_bytes + x_offset;
            let dst_end = dst_start + row_bytes;
            strip[dst_start..dst_end].copy_from_slice(&thumb[src_start..src_end]);
        }
    }

    Ok(Arc::new(UnsafeMutex::new(VideoFrameBuffer {
        width: strip_width,
        height: thumb_height,
        rgba: strip,
        pts_samples: clip.start,
    })))
}

pub fn decode_frame_at_sample(
    clip: &VideoClipData,
    sample_rate: f64,
    target_sample: usize,
) -> Result<Arc<UnsafeMutex<VideoFrameBuffer>>, String> {
    let mut decoder = VideoDecoderState::new(clip.clone(), sample_rate)?;
    decoder.decode_frame_at_sample(target_sample)
}

pub fn estimate_frame_interval_samples(
    clip: &VideoClipData,
    sample_rate: f64,
) -> Result<usize, String> {
    ffmpeg_init().map_err(|e| format!("ffmpeg init failed: {e}"))?;

    let input = format::input(&clip.path).map_err(|e| format!("open video failed: {e}"))?;
    let Some(stream) = input.streams().best(media::Type::Video) else {
        return Err("no video stream found".to_string());
    };
    let rate = {
        let avg = stream.avg_frame_rate();
        if avg.numerator() > 0 && avg.denominator() > 0 {
            avg
        } else {
            let nominal = stream.rate();
            if nominal.numerator() > 0 && nominal.denominator() > 0 {
                nominal
            } else {
                ffmpeg_next::Rational(25, 1)
            }
        }
    };
    let fps = f64::from(rate.numerator().max(1)) / f64::from(rate.denominator().max(1));
    Ok((sample_rate.max(1.0) / fps.max(1.0)).round().max(1.0) as usize)
}
