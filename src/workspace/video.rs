use crate::{message::Message, state::State};
use iced::{
    Background, Border, Color, ContentFit, Element, Length,
    widget::{container, image, mouse_area, row, text},
};

pub struct Video {
    state: State,
}

impl Video {
    pub fn new(state: State) -> Self {
        Self { state }
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
        frame: Option<
            &std::sync::Arc<
                maolan_engine::mutex::UnsafeMutex<maolan_engine::message::VideoFrameBuffer>,
            >,
        >,
        unavailable: &'static str,
        fit: ContentFit,
    ) -> Element<'static, Message> {
        if let Some(frame) = frame {
            let frame = frame.lock();
            if frame.width > 0 && frame.height > 0 && !frame.rgba.is_empty() {
                return image(image::Handle::from_rgba(
                    frame.width,
                    frame.height,
                    frame.rgba.clone(),
                ))
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(fit)
                .into();
            }
        }

        container(text(unavailable).size(18))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }

    pub fn view(
        &self,
        split_resize_hovered: bool,
        split_secondary_resize_hovered: bool,
    ) -> Element<'_, Message> {
        let state = self.state.blocking_read();
        let selected_name = state.selected.iter().next().cloned();
        let selected_track = selected_name.as_ref().and_then(|name| {
            state
                .tracks
                .iter()
                .find(|track| &track.name == name && track.video.is_some())
        });
        let track = selected_name
            .as_ref()
            .and_then(|name| {
                state
                    .tracks
                    .iter()
                    .find(|track| &track.name == name && track.video.is_some())
            })
            .or_else(|| state.tracks.iter().find(|track| track.video.is_some()));

        let content: Element<'_, Message> = if let Some(track) = track {
            let video = track
                .video
                .as_ref()
                .expect("video track lookup guarantees clip");
            if let Some(selected_track) = selected_track {
                let selected_video = selected_track
                    .video
                    .as_ref()
                    .expect("selected video track lookup guarantees clip");
                let left_width = state.video_preview_left_width.max(160.0);
                let middle_width = state.video_preview_middle_width.max(160.0);
                let right_group = row![
                    container(Self::frame_element(
                        video.current_frame.as_ref(),
                        "Current video preview unavailable",
                        ContentFit::Contain,
                    ))
                    .width(Length::Fixed(middle_width))
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
                        selected_video.current_frame.as_ref(),
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
                    container(Self::frame_element(
                        video.frame.as_ref(),
                        "Video preview not decoded yet",
                        ContentFit::Contain,
                    ))
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
            } else {
                let left_width = state.video_preview_left_width.max(160.0);
                row![
                    container(Self::frame_element(
                        video.frame.as_ref(),
                        "Video preview not decoded yet",
                        ContentFit::Contain,
                    ))
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
                    container(Self::frame_element(
                        video.current_frame.as_ref(),
                        "Current video preview unavailable",
                        ContentFit::Contain,
                    ))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .padding(12)
                    .style(Self::panel_style()),
                ]
                .into()
            }
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
