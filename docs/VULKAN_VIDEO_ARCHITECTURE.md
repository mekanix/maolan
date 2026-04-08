# Vulkan Video Architecture

## Goal

Move video decoding and presentation to a Vulkan-backed pipeline that fits the
current Iced renderer model, while removing video decode ownership from
`maolan-engine`.

The intended end state is:

- `maolan-engine` owns transport timing, not video decode resources.
- App/session state owns video track and clip metadata.
- A UI-side video runtime owns Vulkan decode, frame caches, textures, and
  presentation.
- The app renders current video frames without round-tripping decoded frames
  through CPU RGBA buffers in the normal preview path.

## Current State

The current implementation is CPU-based:

- `engine/src/video.rs`
  FFmpeg software decode and software scaling to RGBA.
- `engine/src/engine.rs`
  Schedules current-frame and preview decode work.
- `engine/src/workers/worker.rs`
  Runs decode jobs in worker threads.
- `engine/src/message.rs`
  Sends `TrackVideoFrame` and `TrackVideoCurrentFrame` with
  `Arc<UnsafeMutex<VideoFrameBuffer>>`.
- `src/workspace/video.rs`
  Displays video by converting RGBA bytes into `image::Handle::from_rgba(...)`.

Important observation:

- Transport and timing data already cross the engine/UI boundary.
- Video clip layout and metadata already live in app state.
- The app already mirrors enough timing state to drive a UI-owned video system.

## Why Not Engine-Owned Vulkan Images

The engine should not hand the UI "a GPU location" for decoded frames.

A GPU image is only meaningful relative to:

- a specific `VkDevice`
- memory allocation strategy
- queue family ownership
- image layout state
- synchronization primitives
- texture lifetime rules

Since Iced already owns rendering, the safest architecture is for the final
presentable video textures to be owned by a UI-side Vulkan runtime that lives
next to presentation code.

## Target Architecture

### Responsibilities

`maolan-engine`:

- transport position
- playing/paused/stopped state
- loop range
- playback sample rate
- timing notifications already consumed by the UI

App/session state:

- video track layout
- clip metadata
- selected clip/track state
- session-relative media paths

UI-side video runtime:

- Vulkan device integration strategy
- FFmpeg Vulkan hardware decode setup
- decoder/session lifetime
- frame cache and prefetch
- texture pool management
- color conversion path
- synchronization with rendering
- fallback to CPU decode when Vulkan is unavailable

### High-Level Flow

1. Engine publishes transport timing as it already does.
2. UI observes current transport sample and relevant clip state.
3. UI video runtime decides which clip/frame is needed.
4. Vulkan backend decodes into hardware frames.
5. Runtime converts to a presentable sampled texture format if needed.
6. UI renders using a handle owned by the video runtime, not a CPU RGBA buffer.

## Required Refactor

### 1. Remove Video Decode Ownership From `maolan-engine`

Delete or migrate engine-owned video runtime state:

- `Track.video_frame`
- `Track.video_current_frame`
- `Track.video_decoder`
- `Track.video_current_frame_inflight`
- `Track.video_pending_sample`
- `Track.video_decode_generation`

Likely files:

- `engine/src/track.rs`
- `engine/src/engine.rs`
- `engine/src/workers/worker.rs`
- `engine/src/video.rs`

The engine may still keep transport-driven notifications if they are useful, but
it should stop owning decode resources and stop producing decoded frame buffers
for preview.

### 2. Stop Sending CPU Video Frames Through Engine Actions

Current actions are CPU-buffer oriented:

- `TrackVideoFrame`
- `RequestTrackVideoFrame`
- `RequestTrackVideoCurrentFrame`
- `TrackVideoCurrentFrame`

These should be removed or reduced to UI-local behavior.

If an app-level message is still needed, it should carry:

- clip identity
- frame identity or generation
- readiness state

It should not carry raw Vulkan objects across arbitrary layers.

### 3. Create a UI-Owned Video Runtime

Add a new app-side module, for example:

- `src/video_runtime/mod.rs`
- `src/video_runtime/backend.rs`
- `src/video_runtime/vulkan.rs`
- `src/video_runtime/cpu.rs`
- `src/video_runtime/cache.rs`
- `src/video_runtime/presenter.rs`

Suggested responsibilities:

- `backend.rs`
  Common trait and shared types.
- `vulkan.rs`
  FFmpeg Vulkan hwaccel integration and texture creation.
- `cpu.rs`
  Fallback path based on current software decode logic.
- `cache.rs`
  Frame slot management, generations, and prefetch policy.
- `presenter.rs`
  UI-facing rendering bridge.

### 4. Introduce Stable Frame Abstractions

Replace direct use of `VideoFrameBuffer` in the preview/render path with an enum
or handle-based model, for example:

```rust
pub enum VideoFrameRef {
    Cpu(VideoFrameBuffer),
    Gpu(VideoFrameHandle),
}
```

Where `VideoFrameHandle` is an app-local opaque identifier into a runtime-owned
frame registry or texture pool.

The handle should carry only stable metadata such as:

- frame slot id
- generation
- pts in samples
- width/height

It should not expose raw Vulkan handles to unrelated code.

### 5. Add Backend Selection and Fallback

Implement:

- Vulkan preferred when available and initialized successfully
- CPU fallback when unavailable or unsupported
- graceful fallback on per-file or per-codec failures

This should allow incremental adoption without breaking current video support.

### 6. Move Preview Thumbnail Generation Out of Engine

Current preview strip decode also lives in `engine/src/video.rs`.

That work should move next to the UI-owned video runtime or to a separate
app-side media helper module. It can remain CPU-based initially even if current
frame presentation becomes GPU-based first.

### 7. Keep Export Separate

Realtime video preview and offline export should not be forced into the same
implementation.

Recommended approach:

- UI preview path: Vulkan-backed, low-latency, texture-oriented
- Export path: separate offline pipeline, allowed to use CPU readback or a
  different encode path if needed

Do not over-couple preview architecture to export architecture.

## Incremental Migration Plan

### Phase 1: Boundary Cleanup

- document engine as transport owner only
- stop adding new video behavior to `maolan-engine`
- move preview decode requests out of engine-driven actions
- keep current CPU path working during transition

### Phase 2: UI Runtime Skeleton

- add `src/video_runtime/`
- introduce backend trait and runtime manager
- make current CPU decode path conform to the new runtime API
- switch UI code to request frames from the runtime instead of the engine

### Phase 3: Vulkan Backend

- create Vulkan backend initialization
- bind FFmpeg Vulkan hwdevice/hwframes context
- decode hardware frames
- convert to a presentation-friendly texture format
- return `VideoFrameHandle` from the runtime

### Phase 4: Rendering Integration

- replace `image::Handle::from_rgba(...)` usage for active preview frames
- render video via runtime-owned textures
- keep CPU fallback path for unsupported systems

### Phase 5: Cleanup

- remove obsolete engine video decode code
- remove obsolete engine messages for frame transport
- trim duplicated video state across engine and app

## Open Technical Questions

- How should the app integrate with the Vulkan device/context used by Iced?
- Does the chosen Iced renderer path expose enough hooks for external textures?
- Should decoder-native YUV images be sampled directly, or should the runtime
  always convert to a standard RGBA texture before presentation?
- What frame cache size and prefetch policy are appropriate for scrubbing versus
  playback?
- What is the fallback plan for systems where FFmpeg Vulkan hwaccel is present
  but unreliable?

## Recommended Near-Term Decision

Before writing Vulkan decode code, first commit to this architectural rule:

Video decode and presentation resources are app-side concerns, not
`maolan-engine` concerns.

That decision simplifies the rest of the work and avoids trying to pass GPU
resources across the wrong boundary.
