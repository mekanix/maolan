use crate::{
    message::{VideoClipData, VideoFrameBuffer},
    mutex::UnsafeMutex,
};
use ffmpeg_next::{
    codec, format, frame, media,
    software::scaling::{context::Context as ScalingContext, flag::Flags},
    util::format::pixel::Pixel,
};
use std::sync::{Arc, OnceLock};

const PREVIEW_THUMB_HEIGHT: u32 = 48;
const PREVIEW_MAX_THUMBS: usize = 8;

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
    ffmpeg_init().map_err(|e| format!("ffmpeg init failed: {e}"))?;

    let mut input = format::input(&clip.path).map_err(|e| format!("open video failed: {e}"))?;
    let Some(stream) = input.streams().best(media::Type::Video) else {
        return Err("no video stream found".to_string());
    };
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let context = codec::Context::from_parameters(stream.parameters())
        .map_err(|e| format!("video codec params failed: {e}"))?;
    let mut decoder = context
        .decoder()
        .video()
        .map_err(|e| format!("video decoder open failed: {e}"))?;

    let width = decoder.width().max(1);
    let height = decoder.height().max(1);
    let mut scaler = ScalingContext::get(
        decoder.format(),
        width,
        height,
        Pixel::RGBA,
        width,
        height,
        Flags::BILINEAR,
    )
    .map_err(|e| format!("video scaler init failed: {e}"))?;

    let clip_local_sample = target_sample
        .saturating_sub(clip.start)
        .saturating_add(clip.offset)
        .min(clip.offset.saturating_add(clip.length));
    let target_seconds = clip_local_sample as f64 / sample_rate.max(1.0);

    let mut decoded = frame::Video::empty();
    let mut rgba = frame::Video::empty();
    let mut fallback = None::<VideoFrameBuffer>;

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

            let pts = decoded.timestamp().unwrap_or_default();
            let frame_seconds = (pts as f64) * f64::from(time_base);
            let pts_samples = (frame_seconds * sample_rate.max(1.0)).max(0.0) as usize;
            let frame_buffer = VideoFrameBuffer {
                width,
                height,
                rgba: rgba_pixels(&rgba, width, height),
                pts_samples,
            };

            if fallback.is_none() {
                fallback = Some(frame_buffer.clone());
            }
            if frame_seconds >= target_seconds {
                return Ok(Arc::new(UnsafeMutex::new(frame_buffer)));
            }
        }
    }

    fallback
        .map(|frame| Arc::new(UnsafeMutex::new(frame)))
        .ok_or_else(|| "no decoded video frame available".to_string())
}
