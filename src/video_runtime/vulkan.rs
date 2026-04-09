use crate::{
    message::Message,
    state::VideoClip,
    video_runtime::{
        backend::VideoBackend,
        cpu::{decode_frame_at_sample, decode_iframe_preview_strip},
        registry::{RegisteredVideoTextureSource, VideoTextureRegistry},
        types::{VideoFrameMetadata, VideoFrameRef, VideoRuntimeBackend, VideoTextureHandle},
    },
};
use ffmpeg_next::{
    Rational, codec, ffi, format, frame, media,
    software::scaling::{context::Context as ScalingContext, flag::Flags},
    util::format::pixel::Pixel,
};
use iced::Task;
use maolan_engine::{
    message::{VideoClipData, VideoFrameBuffer},
    mutex::UnsafeMutex,
};
use std::{
    collections::{HashMap, HashSet},
    ffi::CStr,
    fmt,
    sync::OnceLock,
    sync::{Arc, Mutex},
};

const PREVIEW_THUMB_HEIGHT: u32 = 48;
const PREVIEW_MAX_THUMBS: usize = 8;

trait FrameProducer: Send + Sync {
    fn label(&self) -> &'static str;

    fn decode_preview(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
    ) -> Result<ProducedFrame, String>;

    fn decode_current(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
        sample: usize,
    ) -> Result<ProducedFrame, String>;

    fn retain_clip_keys(&self, _clip_keys: &HashSet<String>) {}
}

struct ProducedFrame {
    frame: Arc<UnsafeMutex<VideoFrameBuffer>>,
    producer_label: String,
    fallback_reason: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct HardwareDeviceSpec {
    producer_label: &'static str,
    ffmpeg_label: &'static str,
    device_type: ffi::AVHWDeviceType,
}

const VULKAN_DEVICE_SPEC: HardwareDeviceSpec = HardwareDeviceSpec {
    producer_label: "vulkan-hardware",
    ffmpeg_label: "vulkan",
    device_type: ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
};

const VAAPI_DEVICE_SPEC: HardwareDeviceSpec = HardwareDeviceSpec {
    producer_label: "vaapi-upload",
    ffmpeg_label: "vaapi",
    device_type: ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VulkanFrameProducerKind {
    CpuUpload,
    Hardware,
    #[default]
    Auto,
}

impl VulkanFrameProducerKind {
    pub const ALL: [Self; 3] = [Self::Auto, Self::CpuUpload, Self::Hardware];
}

impl fmt::Display for VulkanFrameProducerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => write!(f, "Auto"),
            Self::CpuUpload => write!(f, "CPU Upload"),
            Self::Hardware => write!(f, "Hardware"),
        }
    }
}

#[derive(Debug, Default)]
struct CpuUploadProducer;

impl FrameProducer for CpuUploadProducer {
    fn label(&self) -> &'static str {
        "cpu-upload"
    }

    fn decode_preview(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
    ) -> Result<ProducedFrame, String> {
        let frame = decode_iframe_preview_strip(clip, sample_rate)?;
        Ok(ProducedFrame {
            frame,
            producer_label: self.label().to_string(),
            fallback_reason: None,
        })
    }

    fn decode_current(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
        sample: usize,
    ) -> Result<ProducedFrame, String> {
        let frame = decode_frame_at_sample(clip, sample_rate, sample)?;
        Ok(ProducedFrame {
            frame,
            producer_label: self.label().to_string(),
            fallback_reason: None,
        })
    }

    fn retain_clip_keys(&self, _clip_keys: &HashSet<String>) {}
}

#[derive(Default)]
struct HardwareDecodeProducer {
    decoders: Mutex<HashMap<String, HardwareDecoderState>>,
}

#[derive(Default)]
struct VaapiDecodeProducer {
    decoders: Mutex<HashMap<String, HardwareDecoderState>>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct HardwareProbeResult {
    codec_name: String,
    hw_pix_fmt: Option<ffi::AVPixelFormat>,
    hw_pix_fmt_name: Option<String>,
    device_method: bool,
    frames_method: bool,
}

#[derive(Debug)]
struct HardwareFormatSelection {
    hw_pix_fmt: ffi::AVPixelFormat,
    requires_frames_ctx: bool,
}

struct OpenedHardwareDecoder {
    input: format::context::Input,
    stream_index: usize,
    time_base: Rational,
    decoder: codec::decoder::Video,
    probe: HardwareProbeResult,
}

struct HardwareDecoderState {
    device: HardwareDeviceSpec,
    clip: VideoClipData,
    sample_rate: f64,
    input: format::context::Input,
    stream_index: usize,
    time_base: Rational,
    decoder: codec::decoder::Video,
    probe: HardwareProbeResult,
    decoded: frame::Video,
    transferred: frame::Video,
    rgba: frame::Video,
    scaler: Option<(Pixel, u32, u32, ScalingContext)>,
    current_seconds: Option<f64>,
    last_frame: Option<VideoFrameBuffer>,
}

unsafe impl Send for HardwareDecoderState {}
unsafe impl Sync for HardwareDecoderState {}

#[derive(Debug)]
struct HwDeviceRef(*mut ffi::AVBufferRef);

impl HwDeviceRef {
    fn create(spec: HardwareDeviceSpec) -> Result<Self, String> {
        let mut device = std::ptr::null_mut();
        let result = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut device,
                spec.device_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if result < 0 {
            return Err(format!(
                "av_hwdevice_ctx_create({}) failed: {}",
                spec.ffmpeg_label,
                ffmpeg_error_string(result),
            ));
        }
        if device.is_null() {
            return Err(format!(
                "av_hwdevice_ctx_create({}) returned a null device",
                spec.ffmpeg_label
            ));
        }
        Ok(Self(device))
    }
}

impl Drop for HwDeviceRef {
    fn drop(&mut self) {
        unsafe {
            ffi::av_buffer_unref(&mut self.0);
        }
    }
}

fn ffmpeg_init() -> Result<(), ffmpeg_next::Error> {
    static RESULT: OnceLock<Result<(), ffmpeg_next::Error>> = OnceLock::new();
    *RESULT.get_or_init(ffmpeg_next::init)
}

fn ffmpeg_error_string(code: i32) -> String {
    ffmpeg_next::Error::from(code).to_string()
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

fn av_pix_fmt_name(format: ffi::AVPixelFormat) -> Option<String> {
    let name = unsafe { ffi::av_get_pix_fmt_name(format) };
    if name.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

#[allow(dead_code)]
fn has_hwdevice_support(device_type: ffi::AVHWDeviceType) -> bool {
    let mut current = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;
    loop {
        current = unsafe { ffi::av_hwdevice_iterate_types(current) };
        if current == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            return false;
        }
        if current == device_type {
            return true;
        }
    }
}

fn codec_supports_hwdevice(
    codec: codec::codec::Codec,
    device: HardwareDeviceSpec,
) -> Result<HardwareProbeResult, String> {
    let codec_name = codec.name().to_string();
    let mut index = 0;
    let mut hw_pix_fmt = None;
    let mut hw_pix_fmt_name = None;
    let mut device_method = false;
    let mut frames_method = false;

    loop {
        let config = unsafe { ffi::avcodec_get_hw_config(codec.as_ptr(), index) };
        if config.is_null() {
            break;
        }

        let config = unsafe { &*config };
        if config.device_type == device.device_type {
            hw_pix_fmt = Some(config.pix_fmt);
            hw_pix_fmt_name = av_pix_fmt_name(config.pix_fmt);
            let methods = config.methods;
            device_method |= (methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32) != 0;
            frames_method |= (methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX as i32) != 0;
        }

        index += 1;
    }

    if !device_method && !frames_method {
        return Err(format!(
            "codec `{codec_name}` exposes no {} hardware configuration",
            device.ffmpeg_label
        ));
    }

    Ok(HardwareProbeResult {
        codec_name,
        hw_pix_fmt,
        hw_pix_fmt_name,
        device_method,
        frames_method,
    })
}

fn find_hwdevice_decoder(
    codec_id: codec::Id,
    device: HardwareDeviceSpec,
) -> Result<(codec::codec::Codec, HardwareProbeResult), String> {
    let mut iterator_state = std::ptr::null_mut();
    let mut fallback_reason = None::<String>;

    loop {
        let codec_ptr = unsafe { ffi::av_codec_iterate(&mut iterator_state) };
        if codec_ptr.is_null() {
            break;
        }

        let codec = unsafe { codec::codec::Codec::wrap(codec_ptr) };
        if !codec.is_decoder() || codec.medium() != media::Type::Video || codec.id() != codec_id {
            continue;
        }

        match codec_supports_hwdevice(codec, device) {
            Ok(probe) => return Ok((codec, probe)),
            Err(reason) => {
                if fallback_reason.is_none() {
                    fallback_reason = Some(reason);
                }
            }
        }
    }

    if let Some(reason) = fallback_reason {
        Err(reason)
    } else {
        Err(format!(
            "no video decoder found for codec id {:?} with {} hardware support",
            codec_id, device.ffmpeg_label
        ))
    }
}

#[allow(dead_code)]
fn probe_hardware_decode_support(
    clip: &VideoClipData,
    device: HardwareDeviceSpec,
) -> Result<HardwareProbeResult, String> {
    ffmpeg_init().map_err(|e| format!("ffmpeg init failed: {e}"))?;

    if !has_hwdevice_support(device.device_type) {
        return Err(format!(
            "FFmpeg was built without {} hwdevice support",
            device.ffmpeg_label
        ));
    }

    let input = format::input(&clip.path).map_err(|e| format!("open video failed: {e}"))?;
    let Some(stream) = input.streams().best(media::Type::Video) else {
        return Err("no video stream found".to_string());
    };

    let parameters = stream.parameters();
    let (_, probe) = find_hwdevice_decoder(parameters.id(), device)?;

    let _device = HwDeviceRef::create(device)?;

    Ok(probe)
}

unsafe extern "C" fn select_vulkan_pixel_format(
    avctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    unsafe {
        if avctx.is_null() || formats.is_null() {
            return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
        }

        let selection = (*avctx).opaque.cast::<HardwareFormatSelection>();
        if selection.is_null() {
            return ffi::avcodec_default_get_format(avctx, formats);
        }
        let selection = &*selection;

        let mut current = formats;
        while *current != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *current == selection.hw_pix_fmt {
                if selection.requires_frames_ctx {
                    let mut frames = std::ptr::null_mut();
                    let mut device_ref = ffi::av_buffer_ref((*avctx).hw_device_ctx.cast_const());
                    if device_ref.is_null() {
                        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
                    }

                    let parameters_result = ffi::avcodec_get_hw_frames_parameters(
                        avctx,
                        device_ref,
                        selection.hw_pix_fmt,
                        &mut frames,
                    );
                    ffi::av_buffer_unref(&mut device_ref);
                    if parameters_result < 0 || frames.is_null() {
                        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
                    }

                    let init_result = ffi::av_hwframe_ctx_init(frames);
                    if init_result < 0 {
                        ffi::av_buffer_unref(&mut frames);
                        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
                    }

                    if !(*avctx).hw_frames_ctx.is_null() {
                        ffi::av_buffer_unref(&mut (*avctx).hw_frames_ctx);
                    }
                    (*avctx).hw_frames_ctx = frames;
                }

                return selection.hw_pix_fmt;
            }
            current = current.add(1);
        }

        ffi::avcodec_default_get_format(avctx, formats)
    }
}

fn open_hardware_decoder(
    clip: &VideoClipData,
    device: HardwareDeviceSpec,
) -> Result<OpenedHardwareDecoder, String> {
    ffmpeg_init().map_err(|e| format!("ffmpeg init failed: {e}"))?;

    let input = format::input(&clip.path).map_err(|e| format!("open video failed: {e}"))?;
    let Some(stream) = input.streams().best(media::Type::Video) else {
        return Err("no video stream found".to_string());
    };
    let parameters = stream.parameters();
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let (codec, probe) = find_hwdevice_decoder(parameters.id(), device)?;
    let hw_pix_fmt = probe.hw_pix_fmt.ok_or_else(|| {
        format!(
            "codec `{}` has no {} hw pixel format",
            probe.codec_name, device.ffmpeg_label
        )
    })?;

    let device_ref = HwDeviceRef::create(device)?;
    let mut context = codec::Context::from_parameters(parameters)
        .map_err(|e| format!("video codec params failed: {e}"))?;

    unsafe {
        let avctx = context.as_mut_ptr();
        let hw_device_ref = ffi::av_buffer_ref(device_ref.0.cast_const());
        if hw_device_ref.is_null() {
            return Err(format!(
                "failed to duplicate {} hw device reference",
                device.ffmpeg_label
            ));
        }
        (*avctx).hw_device_ctx = hw_device_ref;
        (*avctx).get_format = Some(select_vulkan_pixel_format);

        let selection = Box::new(HardwareFormatSelection {
            hw_pix_fmt,
            requires_frames_ctx: probe.frames_method && !probe.device_method,
        });
        (*avctx).opaque = Box::into_raw(selection).cast();

        let open_result = ffi::avcodec_open2(avctx, codec.as_ptr(), std::ptr::null_mut());

        let selection_ptr = (*avctx).opaque.cast::<HardwareFormatSelection>();
        (*avctx).opaque = std::ptr::null_mut();
        if !selection_ptr.is_null() {
            drop(Box::from_raw(selection_ptr));
        }

        if open_result < 0 {
            return Err(format!(
                "avcodec_open2 for {} hw decode failed: {}",
                device.ffmpeg_label,
                ffmpeg_error_string(open_result)
            ));
        }
    }

    let decoder = codec::decoder::Opened(codec::decoder::Decoder(context))
        .video()
        .map_err(|e| format!("opened Vulkan decoder is not a video decoder: {e}"))?;

    Ok(OpenedHardwareDecoder {
        input,
        stream_index,
        time_base,
        decoder,
        probe,
    })
}

fn build_frame_buffer(
    frame: &frame::Video,
    time_base: Rational,
    sample_rate: f64,
    scaler: &mut Option<(Pixel, u32, u32, ScalingContext)>,
    rgba: &mut frame::Video,
) -> Result<VideoFrameBuffer, String> {
    let src_format = frame.format();
    let width = frame.width().max(1);
    let height = frame.height().max(1);
    let needs_scaler = match scaler.as_ref() {
        Some((format, w, h, _)) => *format != src_format || *w != width || *h != height,
        None => true,
    };
    if needs_scaler {
        let next = ScalingContext::get(
            src_format,
            width,
            height,
            Pixel::RGBA,
            width,
            height,
            Flags::BILINEAR,
        )
        .map_err(|e| format!("video scaler init failed: {e}"))?;
        *scaler = Some((src_format, width, height, next));
    }

    let (_, _, _, scaler) = scaler
        .as_mut()
        .ok_or_else(|| "video scaler unavailable".to_string())?;
    scaler
        .run(frame, rgba)
        .map_err(|e| format!("convert video frame failed: {e}"))?;

    let pts = frame.timestamp().unwrap_or_default();
    let frame_seconds = (pts as f64) * f64::from(time_base);
    Ok(VideoFrameBuffer {
        width,
        height,
        rgba: rgba_pixels(rgba, width, height),
        pts_samples: (frame_seconds * sample_rate.max(1.0)).max(0.0) as usize,
    })
}

fn decode_transferred_or_software_frame(
    state: &mut HardwareDecoderState,
    hw_pix_fmt: ffi::AVPixelFormat,
) -> Result<VideoFrameBuffer, String> {
    let source = if state.decoded.format() == Pixel::from(hw_pix_fmt) {
        unsafe {
            ffi::av_frame_unref(state.transferred.as_mut_ptr());
            let transfer_result = ffi::av_hwframe_transfer_data(
                state.transferred.as_mut_ptr(),
                state.decoded.as_ptr(),
                0,
            );
            if transfer_result < 0 {
                return Err(format!(
                    "transfer hardware frame failed: {}",
                    ffmpeg_error_string(transfer_result)
                ));
            }
        }
        &state.transferred
    } else {
        &state.decoded
    };

    build_frame_buffer(
        source,
        state.time_base,
        state.sample_rate,
        &mut state.scaler,
        &mut state.rgba,
    )
}

fn append_preview_thumbs(
    thumbs: &mut Vec<(u32, u32, Vec<u8>)>,
    preview_scaler: &mut Option<(u32, u32, u32, u32, ScalingContext)>,
    requested_samples: &[usize],
    next_index: &mut usize,
    clip: &VideoClipData,
    sample_rate: f64,
    frame: &VideoFrameBuffer,
) -> Result<(), String> {
    while *next_index < requested_samples.len() {
        let requested_sample = requested_samples[*next_index];
        let requested_seconds = requested_sample
            .saturating_sub(clip.start)
            .saturating_add(clip.offset)
            .min(clip.offset.saturating_add(clip.length)) as f64
            / sample_rate.max(1.0);
        let frame_seconds = frame.pts_samples as f64 / sample_rate.max(1.0);
        if frame_seconds + 1.0e-6 < requested_seconds {
            break;
        }

        if frame.width == 0 || frame.height == 0 || frame.rgba.is_empty() {
            *next_index += 1;
            continue;
        }

        let thumb_height = PREVIEW_THUMB_HEIGHT.min(frame.height).max(1);
        let thumb_width = (((frame.width as f64) * (thumb_height as f64) / (frame.height as f64))
            .round() as u32)
            .max(1);
        let needs_scaler = match preview_scaler.as_ref() {
            Some((src_w, src_h, dst_w, dst_h, _)) => {
                *src_w != frame.width
                    || *src_h != frame.height
                    || *dst_w != thumb_width
                    || *dst_h != thumb_height
            }
            None => true,
        };
        if needs_scaler {
            let scaler = ScalingContext::get(
                Pixel::RGBA,
                frame.width,
                frame.height,
                Pixel::RGBA,
                thumb_width,
                thumb_height,
                Flags::BILINEAR,
            )
            .map_err(|e| format!("preview scaler init failed: {e}"))?;
            *preview_scaler = Some((frame.width, frame.height, thumb_width, thumb_height, scaler));
        }

        let mut source = frame::Video::new(Pixel::RGBA, frame.width, frame.height);
        let mut thumb = frame::Video::empty();
        source.data_mut(0).copy_from_slice(&frame.rgba);
        let (_, _, _, _, scaler) = preview_scaler
            .as_mut()
            .ok_or_else(|| "preview scaler unavailable".to_string())?;
        scaler
            .run(&source, &mut thumb)
            .map_err(|e| format!("preview frame scale failed: {e}"))?;
        thumbs.push((
            thumb_width,
            thumb_height,
            rgba_pixels(&thumb, thumb_width, thumb_height),
        ));
        *next_index += 1;
    }

    Ok(())
}

fn decode_hardware_frame_at_sample(
    state: &mut HardwareDecoderState,
    target_sample: usize,
) -> Result<ProducedFrame, String> {
    const FRAME_SEEK_PREROLL_US: i64 = 500_000;

    let clip_local_sample = target_sample
        .saturating_sub(state.clip.start)
        .saturating_add(state.clip.offset)
        .min(state.clip.offset.saturating_add(state.clip.length));
    let target_seconds = clip_local_sample as f64 / state.sample_rate.max(1.0);
    let target_us = (target_seconds * 1_000_000.0).max(0.0) as i64;
    let seek_us = target_us.saturating_sub(FRAME_SEEK_PREROLL_US);
    let should_seek = state
        .current_seconds
        .is_some_and(|current| target_seconds + 1.0e-6 < current || target_seconds - current > 1.0);
    if should_seek {
        state
            .input
            .seek(seek_us, ..)
            .map_err(|e| format!("video seek failed: {e}"))?;
        state.decoder.flush();
        state.current_seconds = None;
        state.last_frame = None;
    }

    if let Some(frame) = &state.last_frame {
        let last_seconds = frame.pts_samples as f64 / state.sample_rate.max(1.0);
        if last_seconds >= target_seconds {
            return Ok(ProducedFrame {
                frame: Arc::new(UnsafeMutex::new(frame.clone())),
                producer_label: "hardware".to_string(),
                fallback_reason: None,
            });
        }
    }

    let hw_pix_fmt = state.probe.hw_pix_fmt.ok_or_else(|| {
        format!(
            "codec `{}` has no {} hw pixel format",
            state.probe.codec_name, state.device.ffmpeg_label
        )
    })?;
    let mut fallback = state.last_frame.clone();

    let packets = state
        .input
        .packets()
        .filter_map(|(packet_stream, packet)| {
            (packet_stream.index() == state.stream_index).then_some(packet)
        })
        .collect::<Vec<_>>();
    for packet in packets {
        state
            .decoder
            .send_packet(&packet)
            .map_err(|e| format!("send hardware video packet failed: {e}"))?;
        while state.decoder.receive_frame(&mut state.decoded).is_ok() {
            let frame_buffer = decode_transferred_or_software_frame(state, hw_pix_fmt)?;
            state.current_seconds =
                Some(frame_buffer.pts_samples as f64 / state.sample_rate.max(1.0));
            state.last_frame = Some(frame_buffer.clone());
            fallback = Some(frame_buffer.clone());
            if (frame_buffer.pts_samples as f64) / state.sample_rate.max(1.0) + 1.0e-6
                >= target_seconds
            {
                return Ok(ProducedFrame {
                    frame: Arc::new(UnsafeMutex::new(frame_buffer)),
                    producer_label: state.device.producer_label.to_string(),
                    fallback_reason: None,
                });
            }
        }
    }

    state
        .decoder
        .send_eof()
        .map_err(|e| format!("flush hardware video decoder failed: {e}"))?;
    while state.decoder.receive_frame(&mut state.decoded).is_ok() {
        let frame_buffer = decode_transferred_or_software_frame(state, hw_pix_fmt)?;
        state.current_seconds = Some(frame_buffer.pts_samples as f64 / state.sample_rate.max(1.0));
        state.last_frame = Some(frame_buffer.clone());
        fallback = Some(frame_buffer.clone());
        if (frame_buffer.pts_samples as f64) / state.sample_rate.max(1.0) + 1.0e-6 >= target_seconds
        {
            return Ok(ProducedFrame {
                frame: Arc::new(UnsafeMutex::new(frame_buffer)),
                producer_label: state.device.producer_label.to_string(),
                fallback_reason: None,
            });
        }
    }

    fallback
        .map(|frame_buffer| ProducedFrame {
            frame: Arc::new(UnsafeMutex::new(frame_buffer)),
            producer_label: state.device.producer_label.to_string(),
            fallback_reason: None,
        })
        .ok_or_else(|| "no decoded hardware video frame available".to_string())
}

fn decode_hardware_preview_strip(
    state: &mut HardwareDecoderState,
) -> Result<ProducedFrame, String> {
    let thumb_count = PREVIEW_MAX_THUMBS.clamp(1, PREVIEW_MAX_THUMBS);
    let clip_end = state.clip.start.saturating_add(state.clip.length);
    let mut requested_samples = Vec::with_capacity(thumb_count);

    for index in 0..thumb_count {
        let sample = if thumb_count == 1 {
            state.clip.start
        } else {
            let numerator = state.clip.length.saturating_mul(index);
            let denominator = thumb_count.saturating_sub(1).max(1);
            state
                .clip
                .start
                .saturating_add(numerator / denominator)
                .min(clip_end)
        };
        if requested_samples.last().copied() != Some(sample) {
            requested_samples.push(sample);
        }
    }

    if requested_samples.is_empty() {
        return Err("no requested hardware preview samples".to_string());
    }

    let first_sample = requested_samples[0];
    let first_local_sample = first_sample
        .saturating_sub(state.clip.start)
        .saturating_add(state.clip.offset)
        .min(state.clip.offset.saturating_add(state.clip.length));
    let first_seconds = first_local_sample as f64 / state.sample_rate.max(1.0);
    let target_us = (first_seconds * 1_000_000.0).max(0.0) as i64;
    let seek_us = target_us.saturating_sub(500_000);
    state
        .input
        .seek(seek_us, ..)
        .map_err(|e| format!("video seek failed: {e}"))?;
    state.decoder.flush();
    state.current_seconds = None;
    state.last_frame = None;

    let hw_pix_fmt = state.probe.hw_pix_fmt.ok_or_else(|| {
        format!(
            "codec `{}` has no {} hw pixel format",
            state.probe.codec_name, state.device.ffmpeg_label
        )
    })?;

    let mut thumbs = Vec::with_capacity(requested_samples.len());
    let mut next_index = 0usize;
    let mut preview_scaler = None::<(u32, u32, u32, u32, ScalingContext)>;
    let packets = state
        .input
        .packets()
        .filter_map(|(packet_stream, packet)| {
            (packet_stream.index() == state.stream_index).then_some(packet)
        })
        .collect::<Vec<_>>();
    for packet in packets {
        if next_index >= requested_samples.len() {
            break;
        }
        state
            .decoder
            .send_packet(&packet)
            .map_err(|e| format!("send hardware preview packet failed: {e}"))?;
        while state.decoder.receive_frame(&mut state.decoded).is_ok() {
            let frame_buffer = decode_transferred_or_software_frame(state, hw_pix_fmt)?;
            state.current_seconds =
                Some(frame_buffer.pts_samples as f64 / state.sample_rate.max(1.0));
            state.last_frame = Some(frame_buffer.clone());
            append_preview_thumbs(
                &mut thumbs,
                &mut preview_scaler,
                &requested_samples,
                &mut next_index,
                &state.clip,
                state.sample_rate,
                &frame_buffer,
            )?;
            if next_index >= requested_samples.len() {
                break;
            }
        }
    }

    if next_index < requested_samples.len() {
        state
            .decoder
            .send_eof()
            .map_err(|e| format!("flush hardware preview decoder failed: {e}"))?;
        while next_index < requested_samples.len()
            && state.decoder.receive_frame(&mut state.decoded).is_ok()
        {
            let frame_buffer = decode_transferred_or_software_frame(state, hw_pix_fmt)?;
            state.current_seconds =
                Some(frame_buffer.pts_samples as f64 / state.sample_rate.max(1.0));
            state.last_frame = Some(frame_buffer.clone());
            append_preview_thumbs(
                &mut thumbs,
                &mut preview_scaler,
                &requested_samples,
                &mut next_index,
                &state.clip,
                state.sample_rate,
                &frame_buffer,
            )?;
        }
    }

    if thumbs.is_empty() {
        return Err("no decoded hardware preview frame available".to_string());
    }

    let thumb_height = thumbs[0].1;
    let strip_width = thumbs
        .iter()
        .map(|(width, _, _)| *width)
        .sum::<u32>()
        .max(1);
    let strip_row_bytes = (strip_width as usize) * 4;
    let mut strip = vec![0_u8; strip_row_bytes * (thumb_height as usize)];
    let mut x_offset = 0usize;

    for (thumb_width, _, pixels) in &thumbs {
        let row_bytes = (*thumb_width as usize) * 4;
        for row in 0..thumb_height as usize {
            let src_start = row * row_bytes;
            let src_end = src_start + row_bytes;
            let dst_start = row * strip_row_bytes + x_offset;
            let dst_end = dst_start + row_bytes;
            strip[dst_start..dst_end].copy_from_slice(&pixels[src_start..src_end]);
        }
        x_offset += row_bytes;
    }

    Ok(ProducedFrame {
        frame: Arc::new(UnsafeMutex::new(VideoFrameBuffer {
            width: strip_width,
            height: thumb_height,
            rgba: strip,
            pts_samples: state.clip.start,
        })),
        producer_label: state.device.producer_label.to_string(),
        fallback_reason: None,
    })
}

impl HardwareDecodeProducer {
    fn new() -> Self {
        Self::default()
    }

    fn clip_runtime_key(clip: &VideoClipData) -> String {
        format!(
            "{}:{}:{}:{}",
            clip.path, clip.start, clip.length, clip.offset
        )
    }

    fn clip_cache_key(clip: &VideoClipData, sample_rate: f64) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            clip.path,
            clip.start,
            clip.length,
            clip.offset,
            sample_rate.to_bits()
        )
    }

    fn decoder_state<'a>(
        &'a self,
        device: HardwareDeviceSpec,
        clip: &VideoClipData,
        sample_rate: f64,
    ) -> Result<std::sync::MutexGuard<'a, HashMap<String, HardwareDecoderState>>, String> {
        let mut decoders = self
            .decoders
            .lock()
            .map_err(|_| "hardware decoder cache poisoned".to_string())?;
        let key = Self::clip_cache_key(clip, sample_rate);
        if !decoders.contains_key(&key) {
            let opened = open_hardware_decoder(clip, device)?;
            decoders.insert(
                key.clone(),
                HardwareDecoderState {
                    device,
                    clip: clip.clone(),
                    sample_rate,
                    input: opened.input,
                    stream_index: opened.stream_index,
                    time_base: opened.time_base,
                    decoder: opened.decoder,
                    probe: opened.probe,
                    decoded: frame::Video::empty(),
                    transferred: frame::Video::empty(),
                    rgba: frame::Video::empty(),
                    scaler: None,
                    current_seconds: None,
                    last_frame: None,
                },
            );
        }
        Ok(decoders)
    }
}

impl FrameProducer for HardwareDecodeProducer {
    fn label(&self) -> &'static str {
        VULKAN_DEVICE_SPEC.producer_label
    }

    fn decode_preview(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
    ) -> Result<ProducedFrame, String> {
        let key = Self::clip_cache_key(clip, sample_rate);
        let mut decoders = self.decoder_state(VULKAN_DEVICE_SPEC, clip, sample_rate)?;
        let state = decoders
            .get_mut(&key)
            .ok_or_else(|| "hardware decoder state missing after insert".to_string())?;
        decode_hardware_preview_strip(state)
    }

    fn decode_current(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
        sample: usize,
    ) -> Result<ProducedFrame, String> {
        let key = Self::clip_cache_key(clip, sample_rate);
        let mut decoders = self.decoder_state(VULKAN_DEVICE_SPEC, clip, sample_rate)?;
        let state = decoders
            .get_mut(&key)
            .ok_or_else(|| "hardware decoder state missing after insert".to_string())?;
        decode_hardware_frame_at_sample(state, sample)
    }

    fn retain_clip_keys(&self, clip_keys: &HashSet<String>) {
        let Ok(mut decoders) = self.decoders.lock() else {
            return;
        };
        decoders.retain(|_, state| clip_keys.contains(&Self::clip_runtime_key(&state.clip)));
    }
}

impl VaapiDecodeProducer {
    fn clip_runtime_key(clip: &VideoClipData) -> String {
        HardwareDecodeProducer::clip_runtime_key(clip)
    }

    fn clip_cache_key(clip: &VideoClipData, sample_rate: f64) -> String {
        HardwareDecodeProducer::clip_cache_key(clip, sample_rate)
    }

    fn decoder_state<'a>(
        &'a self,
        clip: &VideoClipData,
        sample_rate: f64,
    ) -> Result<std::sync::MutexGuard<'a, HashMap<String, HardwareDecoderState>>, String> {
        let mut decoders = self
            .decoders
            .lock()
            .map_err(|_| "vaapi decoder cache poisoned".to_string())?;
        let key = Self::clip_cache_key(clip, sample_rate);
        if !decoders.contains_key(&key) {
            let opened = open_hardware_decoder(clip, VAAPI_DEVICE_SPEC)?;
            decoders.insert(
                key.clone(),
                HardwareDecoderState {
                    device: VAAPI_DEVICE_SPEC,
                    clip: clip.clone(),
                    sample_rate,
                    input: opened.input,
                    stream_index: opened.stream_index,
                    time_base: opened.time_base,
                    decoder: opened.decoder,
                    probe: opened.probe,
                    decoded: frame::Video::empty(),
                    transferred: frame::Video::empty(),
                    rgba: frame::Video::empty(),
                    scaler: None,
                    current_seconds: None,
                    last_frame: None,
                },
            );
        }
        Ok(decoders)
    }
}

impl FrameProducer for VaapiDecodeProducer {
    fn label(&self) -> &'static str {
        VAAPI_DEVICE_SPEC.producer_label
    }

    fn decode_preview(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
    ) -> Result<ProducedFrame, String> {
        let key = Self::clip_cache_key(clip, sample_rate);
        let mut decoders = self.decoder_state(clip, sample_rate)?;
        let state = decoders
            .get_mut(&key)
            .ok_or_else(|| "vaapi decoder state missing after insert".to_string())?;
        decode_hardware_preview_strip(state)
    }

    fn decode_current(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
        sample: usize,
    ) -> Result<ProducedFrame, String> {
        let key = Self::clip_cache_key(clip, sample_rate);
        let mut decoders = self.decoder_state(clip, sample_rate)?;
        let state = decoders
            .get_mut(&key)
            .ok_or_else(|| "vaapi decoder state missing after insert".to_string())?;
        decode_hardware_frame_at_sample(state, sample)
    }

    fn retain_clip_keys(&self, clip_keys: &HashSet<String>) {
        let Ok(mut decoders) = self.decoders.lock() else {
            return;
        };
        decoders.retain(|_, state| clip_keys.contains(&Self::clip_runtime_key(&state.clip)));
    }
}

#[derive(Debug, Clone)]
enum HardwareDecodeRoute {
    Primary,
    Fallback(String),
}

struct CapabilityAwareFrameProducer {
    primary: Arc<dyn FrameProducer>,
    fallback: Arc<dyn FrameProducer>,
    support_cache: Mutex<HashMap<String, HardwareDecodeRoute>>,
}

impl CapabilityAwareFrameProducer {
    fn new(primary: Arc<dyn FrameProducer>, fallback: Arc<dyn FrameProducer>) -> Self {
        Self {
            primary,
            fallback,
            support_cache: Mutex::new(HashMap::new()),
        }
    }

    fn clip_runtime_key(clip: &VideoClipData) -> String {
        format!(
            "{}:{}:{}:{}",
            clip.path, clip.start, clip.length, clip.offset
        )
    }

    fn route_for_clip(
        &self,
        clip: &VideoClipData,
        probe_device: Option<HardwareDeviceSpec>,
    ) -> HardwareDecodeRoute {
        let key = Self::clip_runtime_key(clip);
        if let Ok(cache) = self.support_cache.lock()
            && let Some(route) = cache.get(&key)
        {
            return route.clone();
        }

        let route = match probe_device {
            Some(device) => match probe_hardware_decode_support(clip, device) {
                Ok(_) => HardwareDecodeRoute::Primary,
                Err(reason) => HardwareDecodeRoute::Fallback(reason),
            },
            None => HardwareDecodeRoute::Primary,
        };

        if let Ok(mut cache) = self.support_cache.lock() {
            cache.insert(key, route.clone());
        }

        route
    }

    fn mark_fallback(&self, clip: &VideoClipData, reason: String) {
        let key = Self::clip_runtime_key(clip);
        if let Ok(mut cache) = self.support_cache.lock() {
            cache.insert(key, HardwareDecodeRoute::Fallback(reason));
        }
    }
}

impl FrameProducer for CapabilityAwareFrameProducer {
    fn label(&self) -> &'static str {
        "capability-aware"
    }

    fn decode_preview(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
    ) -> Result<ProducedFrame, String> {
        match self.route_for_clip(clip, None) {
            HardwareDecodeRoute::Primary => match self.primary.decode_preview(clip, sample_rate) {
                Ok(frame) => Ok(frame),
                Err(primary_err) => {
                    let mut frame = self.fallback.decode_preview(clip, sample_rate)?;
                    frame.fallback_reason =
                        Some(format!("{} -> {}", self.primary.label(), primary_err));
                    Ok(frame)
                }
            },
            HardwareDecodeRoute::Fallback(reason) => {
                let mut frame = self.fallback.decode_preview(clip, sample_rate)?;
                frame.fallback_reason =
                    Some(format!("{} probe -> {}", self.primary.label(), reason));
                Ok(frame)
            }
        }
    }

    fn decode_current(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
        sample: usize,
    ) -> Result<ProducedFrame, String> {
        match self.route_for_clip(clip, None) {
            HardwareDecodeRoute::Primary => {
                match self.primary.decode_current(clip, sample_rate, sample) {
                    Ok(frame) => Ok(frame),
                    Err(primary_err) => {
                        let mut frame = self.fallback.decode_current(clip, sample_rate, sample)?;
                        frame.fallback_reason =
                            Some(format!("{} -> {}", self.primary.label(), primary_err));
                        Ok(frame)
                    }
                }
            }
            HardwareDecodeRoute::Fallback(reason) => {
                let mut frame = self.fallback.decode_current(clip, sample_rate, sample)?;
                frame.fallback_reason =
                    Some(format!("{} probe -> {}", self.primary.label(), reason));
                Ok(frame)
            }
        }
    }

    fn retain_clip_keys(&self, clip_keys: &HashSet<String>) {
        self.primary.retain_clip_keys(clip_keys);
        self.fallback.retain_clip_keys(clip_keys);
        let Ok(mut cache) = self.support_cache.lock() else {
            return;
        };
        cache.retain(|clip_key, _| clip_keys.contains(clip_key));
    }
}

struct ProbedFrameProducer {
    probe_device: HardwareDeviceSpec,
    inner: CapabilityAwareFrameProducer,
}

impl ProbedFrameProducer {
    fn new(
        probe_device: HardwareDeviceSpec,
        primary: Arc<dyn FrameProducer>,
        fallback: Arc<dyn FrameProducer>,
    ) -> Self {
        Self {
            probe_device,
            inner: CapabilityAwareFrameProducer::new(primary, fallback),
        }
    }
}

impl FrameProducer for ProbedFrameProducer {
    fn label(&self) -> &'static str {
        self.inner.primary.label()
    }

    fn decode_preview(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
    ) -> Result<ProducedFrame, String> {
        match self.inner.route_for_clip(clip, Some(self.probe_device)) {
            HardwareDecodeRoute::Primary => {
                eprintln!(
                    "[video-runtime] trying {} preview decode for {}",
                    self.inner.primary.label(),
                    clip.path
                );
                match self.inner.primary.decode_preview(clip, sample_rate) {
                    Ok(frame) => Ok(frame),
                    Err(primary_err) => {
                        self.inner.mark_fallback(clip, primary_err.clone());
                        eprintln!(
                            "[video-runtime] {} preview failed for {}: {}",
                            self.inner.primary.label(),
                            clip.path,
                            primary_err
                        );
                        let mut frame = self.inner.fallback.decode_preview(clip, sample_rate)?;
                        frame.fallback_reason =
                            Some(format!("{} -> {}", self.inner.primary.label(), primary_err));
                        Ok(frame)
                    }
                }
            }
            HardwareDecodeRoute::Fallback(reason) => {
                eprintln!(
                    "[video-runtime] {} preview probe failed for {}: {}",
                    self.inner.primary.label(),
                    clip.path,
                    reason
                );
                let mut frame = self.inner.fallback.decode_preview(clip, sample_rate)?;
                frame.fallback_reason = Some(format!(
                    "{} probe -> {}",
                    self.inner.primary.label(),
                    reason
                ));
                Ok(frame)
            }
        }
    }

    fn decode_current(
        &self,
        clip: &VideoClipData,
        sample_rate: f64,
        sample: usize,
    ) -> Result<ProducedFrame, String> {
        match self.inner.route_for_clip(clip, Some(self.probe_device)) {
            HardwareDecodeRoute::Primary => {
                eprintln!(
                    "[video-runtime] trying {} current decode for {}",
                    self.inner.primary.label(),
                    clip.path
                );
                match self.inner.primary.decode_current(clip, sample_rate, sample) {
                    Ok(frame) => Ok(frame),
                    Err(primary_err) => {
                        self.inner.mark_fallback(clip, primary_err.clone());
                        eprintln!(
                            "[video-runtime] {} current failed for {}: {}",
                            self.inner.primary.label(),
                            clip.path,
                            primary_err
                        );
                        let mut frame =
                            self.inner
                                .fallback
                                .decode_current(clip, sample_rate, sample)?;
                        frame.fallback_reason =
                            Some(format!("{} -> {}", self.inner.primary.label(), primary_err));
                        Ok(frame)
                    }
                }
            }
            HardwareDecodeRoute::Fallback(reason) => {
                eprintln!(
                    "[video-runtime] {} current probe failed for {}: {}",
                    self.inner.primary.label(),
                    clip.path,
                    reason
                );
                let mut frame = self
                    .inner
                    .fallback
                    .decode_current(clip, sample_rate, sample)?;
                frame.fallback_reason = Some(format!(
                    "{} probe -> {}",
                    self.inner.primary.label(),
                    reason
                ));
                Ok(frame)
            }
        }
    }

    fn retain_clip_keys(&self, clip_keys: &HashSet<String>) {
        self.inner.retain_clip_keys(clip_keys);
    }
}

#[derive(Debug, Clone)]
struct DecodeRequestState {
    clip: VideoClipData,
    sample_rate: f64,
    sample: usize,
    generation: u64,
    inflight_generation: Option<u64>,
    last_error: Option<String>,
}

struct DecodeTask {
    producer: Arc<dyn FrameProducer>,
    textures: Arc<Mutex<VideoTextureRegistry>>,
    slots: Arc<Mutex<HashMap<String, VideoTextureHandle>>>,
    requests: Arc<Mutex<HashMap<String, DecodeRequestState>>>,
    clip_key: String,
    clip: VideoClipData,
    sample_rate: f64,
    sample: usize,
    generation: u64,
    preview: bool,
}

#[derive(Debug, Clone)]
pub struct VulkanDeviceContext {
    #[allow(dead_code)]
    pub device_label: String,
    #[allow(dead_code)]
    pub queue_family_index: u32,
}

#[derive(Debug, Clone)]
pub struct VulkanRuntimeConfig {
    #[allow(dead_code)]
    pub device: VulkanDeviceContext,
    #[allow(dead_code)]
    pub decode_queue_family_index: u32,
    #[allow(dead_code)]
    pub presentation_queue_family_index: u32,
}

pub struct VulkanBackend {
    config: Option<VulkanRuntimeConfig>,
    #[allow(dead_code)]
    producer_kind: VulkanFrameProducerKind,
    producer: Arc<dyn FrameProducer>,
    textures: Arc<Mutex<VideoTextureRegistry>>,
    preview_slots: Arc<Mutex<HashMap<String, VideoTextureHandle>>>,
    current_slots: Arc<Mutex<HashMap<String, VideoTextureHandle>>>,
    preview_requests: Arc<Mutex<HashMap<String, DecodeRequestState>>>,
    current_requests: Arc<Mutex<HashMap<String, DecodeRequestState>>>,
}

impl VulkanBackend {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            config: None,
            producer_kind: VulkanFrameProducerKind::CpuUpload,
            producer: Arc::new(CpuUploadProducer),
            textures: Arc::new(Mutex::new(VideoTextureRegistry::new())),
            preview_slots: Arc::new(Mutex::new(HashMap::new())),
            current_slots: Arc::new(Mutex::new(HashMap::new())),
            preview_requests: Arc::new(Mutex::new(HashMap::new())),
            current_requests: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[allow(dead_code)]
    pub fn with_config(config: VulkanRuntimeConfig) -> Self {
        Self {
            config: Some(config),
            producer_kind: VulkanFrameProducerKind::CpuUpload,
            producer: Arc::new(CpuUploadProducer),
            textures: Arc::new(Mutex::new(VideoTextureRegistry::new())),
            preview_slots: Arc::new(Mutex::new(HashMap::new())),
            current_slots: Arc::new(Mutex::new(HashMap::new())),
            preview_requests: Arc::new(Mutex::new(HashMap::new())),
            current_requests: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn is_available(&self) -> bool {
        self.config.is_some()
    }

    pub fn with_producer_kind(
        config: VulkanRuntimeConfig,
        producer_kind: VulkanFrameProducerKind,
    ) -> Self {
        let producer = match producer_kind {
            VulkanFrameProducerKind::CpuUpload => {
                Arc::new(CpuUploadProducer) as Arc<dyn FrameProducer>
            }
            VulkanFrameProducerKind::Hardware => Self::hardware_preferred_producer(),
            VulkanFrameProducerKind::Auto => Self::default_producer_from_env(),
        };
        Self {
            config: Some(config),
            producer_kind,
            producer,
            textures: Arc::new(Mutex::new(VideoTextureRegistry::new())),
            preview_slots: Arc::new(Mutex::new(HashMap::new())),
            current_slots: Arc::new(Mutex::new(HashMap::new())),
            preview_requests: Arc::new(Mutex::new(HashMap::new())),
            current_requests: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[allow(dead_code)]
    fn with_frame_producer(config: VulkanRuntimeConfig, producer: Arc<dyn FrameProducer>) -> Self {
        Self {
            config: Some(config),
            producer_kind: VulkanFrameProducerKind::CpuUpload,
            producer,
            textures: Arc::new(Mutex::new(VideoTextureRegistry::new())),
            preview_slots: Arc::new(Mutex::new(HashMap::new())),
            current_slots: Arc::new(Mutex::new(HashMap::new())),
            preview_requests: Arc::new(Mutex::new(HashMap::new())),
            current_requests: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[allow(dead_code)]
    pub fn config(&self) -> Option<&VulkanRuntimeConfig> {
        self.config.as_ref()
    }

    pub fn texture_registry(&self) -> Arc<Mutex<VideoTextureRegistry>> {
        self.textures.clone()
    }

    #[allow(dead_code)]
    pub fn producer_kind(&self) -> VulkanFrameProducerKind {
        self.producer_kind
    }

    fn hardware_preferred_producer() -> Arc<dyn FrameProducer> {
        Arc::new(ProbedFrameProducer::new(
            VULKAN_DEVICE_SPEC,
            Arc::new(HardwareDecodeProducer::new()),
            Arc::new(ProbedFrameProducer::new(
                VAAPI_DEVICE_SPEC,
                Arc::new(VaapiDecodeProducer::default()),
                Arc::new(CpuUploadProducer),
            )),
        ))
    }

    fn default_producer_from_env() -> Arc<dyn FrameProducer> {
        match std::env::var("MAOLAN_VIDEO_VULKAN_PRODUCER")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("hardware") => Self::hardware_preferred_producer(),
            Some("cpu-upload") => Arc::new(CpuUploadProducer),
            _ => Self::hardware_preferred_producer(),
        }
    }

    fn clip_key(clip: &VideoClip) -> String {
        format!(
            "{}:{}:{}:{}",
            clip.path, clip.start, clip.length, clip.offset
        )
    }

    fn clip_data_key(clip: &VideoClipData) -> String {
        format!(
            "{}:{}:{}:{}",
            clip.path, clip.start, clip.length, clip.offset
        )
    }

    fn parse_clip_key(clip_key: &str) -> Option<(&str, usize, usize, usize)> {
        let mut parts = clip_key.rsplitn(4, ':');
        let offset = parts.next()?.parse().ok()?;
        let length = parts.next()?.parse().ok()?;
        let start = parts.next()?.parse().ok()?;
        let path = parts.next()?;
        Some((path, start, length, offset))
    }

    fn clip_paths_match(request_path: &str, clip_path: &str) -> bool {
        request_path == clip_path
            || request_path.ends_with(clip_path)
            || request_path.ends_with(&format!("/{clip_path}"))
    }

    fn lookup_slot_handle(
        slots: &Arc<Mutex<HashMap<String, VideoTextureHandle>>>,
        clip: &VideoClip,
    ) -> Option<VideoTextureHandle> {
        let clip_key = Self::clip_key(clip);
        let slots = slots.lock().ok()?;
        if let Some(handle) = slots.get(&clip_key).copied() {
            return Some(handle);
        }
        slots.iter().find_map(|(registered_key, handle)| {
            let (path, start, length, offset) = Self::parse_clip_key(registered_key)?;
            (start == clip.start
                && length == clip.length
                && offset == clip.offset
                && Self::clip_paths_match(path, &clip.path))
            .then_some(*handle)
        })
    }

    fn evict_slots(
        slots: &Arc<Mutex<HashMap<String, VideoTextureHandle>>>,
        textures: &Arc<Mutex<VideoTextureRegistry>>,
        clip_keys: &HashSet<String>,
    ) {
        let removed_handles = {
            let Ok(mut slots) = slots.lock() else {
                return;
            };
            let removed = slots
                .iter()
                .filter(|(clip_key, _)| !clip_keys.contains(*clip_key))
                .map(|(clip_key, handle)| (clip_key.clone(), *handle))
                .collect::<Vec<_>>();
            for (clip_key, _) in &removed {
                slots.remove(clip_key);
            }
            removed
        };

        if removed_handles.is_empty() {
            return;
        }

        if let Ok(mut textures) = textures.lock() {
            for (_, handle) in removed_handles {
                let _ = textures.remove(&handle);
            }
        }
    }

    fn evict_requests(
        requests: &Arc<Mutex<HashMap<String, DecodeRequestState>>>,
        clip_keys: &HashSet<String>,
    ) {
        if let Ok(mut requests) = requests.lock() {
            requests.retain(|clip_key, _| clip_keys.contains(clip_key));
        }
    }

    fn frame_source(
        produced: ProducedFrame,
    ) -> Option<(
        VideoFrameMetadata,
        RegisteredVideoTextureSource,
        String,
        Option<String>,
    )> {
        let frame = produced.frame.lock();
        if frame.width == 0 || frame.height == 0 || frame.rgba.is_empty() {
            return None;
        }
        Some((
            VideoFrameMetadata {
                width: frame.width,
                height: frame.height,
                pts_samples: frame.pts_samples,
            },
            RegisteredVideoTextureSource::Rgba8 {
                width: frame.width,
                height: frame.height,
                pixels: frame.rgba.clone(),
            },
            produced.producer_label,
            produced.fallback_reason,
        ))
    }

    fn queue_request(
        requests: &Arc<Mutex<HashMap<String, DecodeRequestState>>>,
        clip_key: &str,
        clip: VideoClipData,
        sample_rate: f64,
        sample: usize,
    ) -> Option<(u64, bool)> {
        let mut requests = requests.lock().ok()?;
        let entry = requests
            .entry(clip_key.to_string())
            .or_insert_with(|| DecodeRequestState {
                clip: clip.clone(),
                sample_rate,
                sample,
                generation: 0,
                inflight_generation: None,
                last_error: None,
            });
        entry.clip = clip;
        entry.sample_rate = sample_rate;
        entry.sample = sample;
        entry.generation = entry.generation.wrapping_add(1);
        entry.last_error = None;
        let generation = entry.generation;
        let should_spawn = entry.inflight_generation.is_none();
        if should_spawn {
            entry.inflight_generation = Some(generation);
        }
        Some((generation, should_spawn))
    }

    fn spawn_decode_task(task: DecodeTask) -> Task<Message> {
        Task::perform(
            async move {
                let result = if task.preview {
                    task.producer.decode_preview(&task.clip, task.sample_rate)
                } else {
                    task.producer
                        .decode_current(&task.clip, task.sample_rate, task.sample)
                };
                let can_commit = task
                    .requests
                    .lock()
                    .ok()
                    .and_then(|requests| requests.get(&task.clip_key).cloned())
                    .is_some_and(|state| state.inflight_generation == Some(task.generation));
                if can_commit {
                    match result {
                        Ok(frame) => {
                            if let Some((metadata, source, producer_label, fallback_reason)) =
                                Self::frame_source(frame)
                            {
                                Self::log_decode_result(
                                    &task.clip_key,
                                    task.preview,
                                    &producer_label,
                                    fallback_reason.as_deref(),
                                );
                                let _ = Self::upsert_texture(
                                    &task.textures,
                                    &task.slots,
                                    task.clip_key.clone(),
                                    metadata,
                                    source,
                                    producer_label,
                                    fallback_reason,
                                );
                            } else if let Ok(mut requests) = task.requests.lock()
                                && let Some(state) = requests.get_mut(&task.clip_key)
                                && state.inflight_generation == Some(task.generation)
                            {
                                state.last_error =
                                    Some("decoded video frame was empty".to_string());
                            }
                        }
                        Err(err) => {
                            eprintln!(
                                "[video-runtime] {} {} decode failed: {}",
                                if task.preview { "preview" } else { "current" },
                                task.clip_key,
                                err
                            );
                            if let Ok(mut requests) = task.requests.lock()
                                && let Some(state) = requests.get_mut(&task.clip_key)
                                && state.inflight_generation == Some(task.generation)
                            {
                                state.last_error = Some(err);
                            }
                        }
                    }
                }
                Message::VideoRuntimeDecodeFinished {
                    clip_key: task.clip_key,
                    preview: task.preview,
                    generation: task.generation,
                }
            },
            std::convert::identity,
        )
    }

    fn frame_ref_from_registry(
        &self,
        slots: &Arc<Mutex<HashMap<String, VideoTextureHandle>>>,
        clip: &VideoClip,
    ) -> Option<VideoFrameRef<'static>> {
        let handle = Self::lookup_slot_handle(slots, clip)?;
        let metadata = self.textures.lock().ok()?.get(&handle)?.metadata;
        Some(VideoFrameRef::Gpu { handle, metadata })
    }

    fn upsert_texture(
        textures: &Arc<Mutex<VideoTextureRegistry>>,
        slots: &Arc<Mutex<HashMap<String, VideoTextureHandle>>>,
        clip_key: String,
        metadata: VideoFrameMetadata,
        source: RegisteredVideoTextureSource,
        producer_label: String,
        fallback_reason: Option<String>,
    ) -> Option<VideoTextureHandle> {
        let mut slots = slots.lock().ok()?;
        let mut textures = textures.lock().ok()?;
        if let Some(handle) = slots.get(&clip_key).copied() {
            if textures
                .replace(
                    handle,
                    metadata,
                    source.clone(),
                    producer_label.clone(),
                    fallback_reason.clone(),
                )
                .is_some()
            {
                Some(handle)
            } else {
                let handle = textures.insert(metadata, source, producer_label, fallback_reason);
                slots.insert(clip_key, handle);
                Some(handle)
            }
        } else {
            let handle = textures.insert(metadata, source, producer_label, fallback_reason);
            slots.insert(clip_key, handle);
            Some(handle)
        }
    }

    fn log_decode_result(
        clip_key: &str,
        preview: bool,
        producer_label: &str,
        fallback_reason: Option<&str>,
    ) {
        let kind = if preview { "preview" } else { "current" };
        match fallback_reason {
            Some(reason) => {
                eprintln!("[video-runtime] {kind} {clip_key} -> {producer_label} ({reason})");
            }
            None => {
                eprintln!("[video-runtime] {kind} {clip_key} -> {producer_label}");
            }
        }
    }

    fn load_state_for_clip_key(
        requests: &Arc<Mutex<HashMap<String, DecodeRequestState>>>,
        clip_key: &str,
    ) -> Option<crate::video_runtime::types::VideoFrameLoadState> {
        let requests = requests.lock().ok()?;
        let state = requests.get(clip_key)?;
        if state.inflight_generation.is_some() {
            Some(crate::video_runtime::types::VideoFrameLoadState::Loading)
        } else {
            state
                .last_error
                .as_ref()
                .cloned()
                .map(crate::video_runtime::types::VideoFrameLoadState::Failed)
        }
    }
}

impl VideoBackend for VulkanBackend {
    fn kind(&self) -> VideoRuntimeBackend {
        VideoRuntimeBackend::Vulkan
    }

    fn preview_frame<'a>(&self, clip: &'a VideoClip) -> Option<VideoFrameRef<'a>> {
        let _config = self.config.as_ref()?;
        self.frame_ref_from_registry(&self.preview_slots, clip)
    }

    fn current_frame<'a>(&self, clip: &'a VideoClip) -> Option<VideoFrameRef<'a>> {
        let _config = self.config.as_ref()?;
        self.frame_ref_from_registry(&self.current_slots, clip)
    }

    fn preview_load_state(
        &self,
        clip: &VideoClip,
    ) -> Option<crate::video_runtime::types::VideoFrameLoadState> {
        let _config = self.config.as_ref()?;
        let clip_key = Self::clip_key(clip);
        if self
            .frame_ref_from_registry(&self.preview_slots, clip)
            .is_some()
        {
            None
        } else {
            Self::load_state_for_clip_key(&self.preview_requests, &clip_key)
        }
    }

    fn current_load_state(
        &self,
        clip: &VideoClip,
    ) -> Option<crate::video_runtime::types::VideoFrameLoadState> {
        let _config = self.config.as_ref()?;
        let clip_key = Self::clip_key(clip);
        if self
            .frame_ref_from_registry(&self.current_slots, clip)
            .is_some()
        {
            None
        } else {
            Self::load_state_for_clip_key(&self.current_requests, &clip_key)
                .or_else(|| Self::load_state_for_clip_key(&self.preview_requests, &clip_key))
        }
    }

    fn request_preview_frame(
        &self,
        _track_name: String,
        clip: VideoClipData,
        sample_rate: f64,
    ) -> Task<Message> {
        if self.config.is_none() {
            return Task::none();
        }

        let clip_key = Self::clip_data_key(&clip);
        let Some((generation, should_spawn)) = Self::queue_request(
            &self.preview_requests,
            &clip_key,
            clip.clone(),
            sample_rate,
            clip.start,
        ) else {
            return Task::none();
        };
        if should_spawn {
            Self::spawn_decode_task(DecodeTask {
                producer: self.producer.clone(),
                textures: self.textures.clone(),
                slots: self.preview_slots.clone(),
                requests: self.preview_requests.clone(),
                clip_key,
                clip: clip.clone(),
                sample_rate,
                sample: clip.start,
                generation,
                preview: true,
            })
        } else {
            Task::none()
        }
    }

    fn request_current_frame(
        &self,
        _track_name: String,
        clip: VideoClipData,
        sample_rate: f64,
        sample: usize,
    ) -> Task<Message> {
        if self.config.is_none() {
            return Task::none();
        }

        let clip_key = Self::clip_data_key(&clip);
        let Some((generation, should_spawn)) = Self::queue_request(
            &self.current_requests,
            &clip_key,
            clip.clone(),
            sample_rate,
            sample,
        ) else {
            return Task::none();
        };
        if should_spawn {
            Self::spawn_decode_task(DecodeTask {
                producer: self.producer.clone(),
                textures: self.textures.clone(),
                slots: self.current_slots.clone(),
                requests: self.current_requests.clone(),
                clip_key,
                clip,
                sample_rate,
                sample,
                generation,
                preview: false,
            })
        } else {
            Task::none()
        }
    }

    fn finish_decode(&self, clip_key: String, preview: bool, generation: u64) -> Task<Message> {
        let requests = if preview {
            self.preview_requests.clone()
        } else {
            self.current_requests.clone()
        };
        let slots = if preview {
            self.preview_slots.clone()
        } else {
            self.current_slots.clone()
        };

        let next = {
            let Ok(mut requests) = requests.lock() else {
                return Task::none();
            };
            let Some(state) = requests.get_mut(&clip_key) else {
                return Task::none();
            };
            if state.inflight_generation != Some(generation) {
                return Task::none();
            }
            if state.generation == generation {
                state.inflight_generation = None;
                None
            } else {
                let next_generation = state.generation;
                state.inflight_generation = Some(next_generation);
                Some((
                    state.clip.clone(),
                    state.sample_rate,
                    state.sample,
                    next_generation,
                ))
            }
        };

        if let Some((clip, sample_rate, sample, next_generation)) = next {
            Self::spawn_decode_task(DecodeTask {
                producer: self.producer.clone(),
                textures: self.textures.clone(),
                slots,
                requests,
                clip_key,
                clip,
                sample_rate,
                sample,
                generation: next_generation,
                preview,
            })
        } else {
            Task::none()
        }
    }

    fn retain_clip_keys(&self, clip_keys: &HashSet<String>) {
        Self::evict_slots(&self.preview_slots, &self.textures, clip_keys);
        Self::evict_slots(&self.current_slots, &self.textures, clip_keys);
        Self::evict_requests(&self.preview_requests, clip_keys);
        Self::evict_requests(&self.current_requests, clip_keys);
        self.producer.retain_clip_keys(clip_keys);
    }
}
