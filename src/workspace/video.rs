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
                .content_fit(ContentFit::Contain)
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

    pub fn view(&self, split_resize_hovered: bool) -> Element<'_, Message> {
        let state = self.state.blocking_read();
        let split_ratio = state.video_preview_split.clamp(0.2, 0.8);
        let selected_name = state.selected.iter().next().cloned();
        let track = selected_name
            .as_ref()
            .and_then(|name| state.tracks.iter().find(|track| &track.name == name))
            .or_else(|| state.tracks.iter().find(|track| track.video.is_some()));

        let content: Element<'_, Message> = if let Some(track) = track {
            if let Some(video) = &track.video {
                row![
                    container(Self::frame_element(
                        video.frame.as_ref(),
                        "Video preview not decoded yet",
                    ))
                    .width(Length::FillPortion((split_ratio * 1000.0) as u16))
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
                    ))
                    .width(Length::FillPortion(((1.0 - split_ratio) * 1000.0) as u16))
                    .height(Length::Fill)
                    .padding(12)
                    .style(Self::panel_style()),
                ]
                .into()
            } else {
                container(text("Selected track has no video clip").size(18))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
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
