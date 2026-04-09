use crate::{message::Message, state::State};
use iced::{
    Background, Border, Color, ContentFit, Element, Length,
    widget::{column, container, image, mouse_area, row, scrollable, text},
};
use std::path::PathBuf;

pub struct Video {
    state: State,
}

impl Video {
    pub fn new(state: State) -> Self {
        Self { state }
    }

    fn backend_label(video_runtime: &crate::video_runtime::VideoRuntime) -> &'static str {
        match video_runtime.backend() {
            crate::video_runtime::types::VideoRuntimeBackend::Cpu => "CPU",
            crate::video_runtime::types::VideoRuntimeBackend::Vulkan => "Vulkan",
        }
    }

    fn load_state_label(
        video_runtime: &crate::video_runtime::VideoRuntime,
        video: &crate::state::VideoClip,
        current: bool,
    ) -> Option<String> {
        let state = if current {
            video_runtime.current_load_state(video)
        } else {
            video_runtime.preview_load_state(video)
        }?;
        Some(match state {
            crate::video_runtime::types::VideoFrameLoadState::Loading => {
                format!("{} • loading", Self::backend_label(video_runtime))
            }
            crate::video_runtime::types::VideoFrameLoadState::Failed(err) => {
                format!("{} • {}", Self::backend_label(video_runtime), err)
            }
        })
    }

    fn panel_style() -> impl Fn(&iced::Theme) -> container::Style {
        |_theme| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.13, 0.10, 0.04, 0.96))),
            border: Border {
                color: Color::from_rgba(0.92, 0.82, 0.34, 0.70),
                width: 1.0,
                radius: 10.0.into(),
            },
            ..container::Style::default()
        }
    }

    fn frame_element(
        video_runtime: &crate::video_runtime::VideoRuntime,
        video: Option<&crate::state::VideoClip>,
        unavailable: &'static str,
        fit: ContentFit,
    ) -> Element<'static, Message> {
        if let Some(video) = video
            && let Some(frame) = video_runtime.current_frame(video)
        {
            if let Some(handle) = video_runtime.image_handle(&frame) {
                return image(handle)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(fit)
                    .into();
            }

            match video_runtime.presentable_frame(&frame) {
                Some(crate::video_runtime::presenter::PresentableFrame::CpuImage(_)) => {}
                Some(crate::video_runtime::presenter::PresentableFrame::GpuTexture(handle)) => {
                    if let crate::video_runtime::types::VideoFrameRef::Gpu { metadata, .. } = frame
                        && let Some(registry) = video_runtime.texture_registry()
                    {
                        return crate::video_runtime::widget::VideoSurface::new(
                            handle, metadata, registry,
                        )
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into();
                    }
                }
                None => {}
            }
        }

        if let Some(video) = video
            && let Some(label) = Self::load_state_label(video_runtime, video, true)
        {
            return container(column![text(unavailable).size(18), text(label).size(12)].spacing(8))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into();
        }

        container(text(unavailable).size(18))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }

    fn session_video_files(session_root: Option<&PathBuf>) -> Vec<String> {
        let Some(session_root) = session_root else {
            return Vec::new();
        };
        let video_dir = session_root.join("video");
        let Ok(entries) = std::fs::read_dir(video_dir) else {
            return Vec::new();
        };
        let mut files = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                entry.file_type().ok().filter(|kind| kind.is_file())?;
                Some(entry.file_name().to_string_lossy().to_string())
            })
            .collect::<Vec<_>>();
        files.sort_unstable();
        files
    }

    fn video_file_list(
        session_root: Option<&PathBuf>,
        selected_video: Option<&crate::state::VideoClip>,
    ) -> Element<'static, Message> {
        let files = Self::session_video_files(session_root);
        let selected_name = selected_video
            .and_then(|video| std::path::Path::new(&video.path).file_name())
            .map(|name| name.to_string_lossy().to_string());

        if files.is_empty() {
            return container(text("No files in session/video").size(18))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into();
        }

        let mut items = column![];
        for file in files {
            let is_selected = selected_name.as_ref().is_some_and(|name| *name == file);
            items = items.push(
                container(text(file).size(16))
                    .width(Length::Fill)
                    .padding([8, 10])
                    .style(move |_theme| container::Style {
                        background: is_selected
                            .then(|| Background::Color(Color::from_rgba(0.92, 0.82, 0.34, 0.18))),
                        ..container::Style::default()
                    }),
            );
        }

        scrollable(items.spacing(4)).into()
    }

    pub fn view(
        &self,
        session_root: Option<&PathBuf>,
        split_resize_hovered: bool,
        split_secondary_resize_hovered: bool,
        video_runtime: &crate::video_runtime::VideoRuntime,
    ) -> Element<'_, Message> {
        let state = self.state.blocking_read();
        let selected_name = state.selected.iter().next().cloned();
        let selected_track = selected_name.as_ref().and_then(|name| {
            state
                .tracks
                .iter()
                .find(|track| &track.name == name && track.video.is_some())
        });
        let fallback_track = if selected_name.is_none() {
            state.tracks.iter().find(|track| track.video.is_some())
        } else {
            None
        };
        let file_list_video = selected_track
            .and_then(|track| track.video.as_ref())
            .or_else(|| fallback_track.and_then(|track| track.video.as_ref()));

        let content: Element<'_, Message> = if let Some(selected_track) = selected_track {
            let selected_video = selected_track
                .video
                .as_ref()
                .expect("selected video track lookup guarantees clip");
            let left_width = state.video_preview_left_width.max(160.0);
            let middle_width = state.video_preview_middle_width.max(160.0);
            let right_group = row![
                container(text(format!("VFX {}", Self::backend_label(video_runtime))).size(24))
                    .width(Length::Fixed(middle_width))
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .padding(12)
                    .style(Self::panel_style()),
                mouse_area(
                    container("")
                        .width(Length::Fixed(3.0))
                        .height(Length::Fill)
                        .style(move |_theme| container::Style {
                            background: Some(Background::Color(Color {
                                r: 0.7,
                                g: 0.7,
                                b: 0.7,
                                a: if split_secondary_resize_hovered {
                                    0.95
                                } else {
                                    0.6
                                },
                            })),
                            ..container::Style::default()
                        }),
                )
                .on_enter(Message::VideoPreviewSplitSecondaryResizeHover(true))
                .on_exit(Message::VideoPreviewSplitSecondaryResizeHover(false))
                .on_press(Message::VideoPreviewSplitSecondaryResizeStart),
                container(Self::frame_element(
                    video_runtime,
                    Some(selected_video),
                    "Selected video preview unavailable",
                    ContentFit::Fill,
                ))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(12)
                .style(Self::panel_style()),
            ]
            .width(Length::Fill);
            row![
                container(Self::video_file_list(session_root, file_list_video))
                    .width(Length::Fixed(left_width))
                    .height(Length::Fill)
                    .padding(12)
                    .style(Self::panel_style()),
                mouse_area(
                    container("")
                        .width(Length::Fixed(3.0))
                        .height(Length::Fill)
                        .style(move |_theme| container::Style {
                            background: Some(Background::Color(Color {
                                r: 0.7,
                                g: 0.7,
                                b: 0.7,
                                a: if split_resize_hovered { 0.95 } else { 0.6 },
                            })),
                            ..container::Style::default()
                        }),
                )
                .on_enter(Message::VideoPreviewSplitResizeHover(true))
                .on_exit(Message::VideoPreviewSplitResizeHover(false))
                .on_press(Message::VideoPreviewSplitResizeStart),
                right_group.width(Length::Fill),
            ]
            .into()
        } else if let Some(video) = fallback_track.and_then(|track| track.video.as_ref()) {
            let left_width = state.video_preview_left_width.max(160.0);
            row![
                container(Self::video_file_list(session_root, file_list_video))
                    .width(Length::Fixed(left_width))
                    .height(Length::Fill)
                    .padding(12)
                    .style(Self::panel_style()),
                mouse_area(
                    container("")
                        .width(Length::Fixed(3.0))
                        .height(Length::Fill)
                        .style(move |_theme| container::Style {
                            background: Some(Background::Color(Color {
                                r: 0.7,
                                g: 0.7,
                                b: 0.7,
                                a: if split_resize_hovered { 0.95 } else { 0.6 },
                            })),
                            ..container::Style::default()
                        }),
                )
                .on_enter(Message::VideoPreviewSplitResizeHover(true))
                .on_exit(Message::VideoPreviewSplitResizeHover(false))
                .on_press(Message::VideoPreviewSplitResizeStart),
                row![
                    container(text(format!("VFX {}", Self::backend_label(video_runtime))).size(24))
                        .width(Length::Fixed(state.video_preview_middle_width.max(160.0)))
                        .height(Length::Fill)
                        .center_x(Length::Fill)
                        .center_y(Length::Fill)
                        .padding(12)
                        .style(Self::panel_style()),
                    mouse_area(
                        container("")
                            .width(Length::Fixed(3.0))
                            .height(Length::Fill)
                            .style(move |_theme| container::Style {
                                background: Some(Background::Color(Color {
                                    r: 0.7,
                                    g: 0.7,
                                    b: 0.7,
                                    a: if split_secondary_resize_hovered {
                                        0.95
                                    } else {
                                        0.6
                                    },
                                })),
                                ..container::Style::default()
                            }),
                    )
                    .on_enter(Message::VideoPreviewSplitSecondaryResizeHover(true))
                    .on_exit(Message::VideoPreviewSplitSecondaryResizeHover(false))
                    .on_press(Message::VideoPreviewSplitSecondaryResizeStart),
                    container(Self::frame_element(
                        video_runtime,
                        Some(video),
                        "Video player unavailable",
                        ContentFit::Contain,
                    ))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .padding(12)
                    .style(Self::panel_style()),
                ]
                .width(Length::Fill),
            ]
            .into()
        } else if selected_name.is_some() {
            let left_width = state.video_preview_left_width.max(160.0);
            row![
                container(Self::video_file_list(session_root, file_list_video))
                    .width(Length::Fixed(left_width))
                    .height(Length::Fill)
                    .padding(12)
                    .style(Self::panel_style()),
                mouse_area(
                    container("")
                        .width(Length::Fixed(3.0))
                        .height(Length::Fill)
                        .style(move |_theme| container::Style {
                            background: Some(Background::Color(Color {
                                r: 0.7,
                                g: 0.7,
                                b: 0.7,
                                a: if split_resize_hovered { 0.95 } else { 0.6 },
                            })),
                            ..container::Style::default()
                        }),
                )
                .on_enter(Message::VideoPreviewSplitResizeHover(true))
                .on_exit(Message::VideoPreviewSplitResizeHover(false))
                .on_press(Message::VideoPreviewSplitResizeStart),
                container(text("Selected track has no video subtrack").size(20))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .padding(12)
                    .style(Self::panel_style()),
            ]
            .into()
        } else {
            container(text("No imported video available").size(18))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        };

        container(content)
            .padding([12, 16])
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}
