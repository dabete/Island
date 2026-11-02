// Release builds run without a console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::{Duration, Instant};

use chrono::{Local, Timelike};
use iced::widget::{container, mouse_area, text};
use iced::window::{self, settings::PlatformSpecific};
use iced::{Background, Border, Color, Element, Length, Point, Size, Subscription, Task, time, theme};

const COLLAPSED: Size = Size::new(110.0, 34.0);
const EXPANDED: Size = Size::new(200.0, 64.0);
// The OS window never changes size: it is big enough for the expanded pill plus a margin,
// and the pill is drawn inside it. Resizing the window every frame flickered.
const WINDOW: Size = Size::new(240.0, 80.0);
const FONT_COLLAPSED: f32 = 16.0;
const FONT_EXPANDED: f32 = 30.0;
const TOP_MARGIN: f32 = 8.0;
// Time constant of the slow-down: bigger = slower. About 4 x this to settle (~350 ms).
const SMOOTHING_SECONDS: f32 = 0.08;
// Closer than this (in pixels) to the goal counts as arrived.
const SNAP_DISTANCE: f32 = 0.5;

// State: everything the app must remember between events.
struct Island {
    time_text: String,
    until_next_minute: Duration,
    hovered: bool,
    // Current drawn size of the pill; the goal is derived from `hovered`.
    size: Size,
    last_frame: Option<Instant>,
}

// Messages: the things that can happen.
#[derive(Debug, Clone)]
enum Message {
    MinuteTick,
    Hovered(bool),
    Frame(Instant),
    // Window id and monitor width, known once the window exists.
    WindowReady(window::Id, f32),
}

fn main() -> iced::Result {
    iced::application(Island::boot, Island::update, Island::view)
        .window(window::Settings {
            size: WINDOW,
            // Real position is set once the monitor width is known.
            position: window::Position::Specific(Point::new(0.0, TOP_MARGIN)),
            decorations: false,
            transparent: true,
            resizable: false,
            level: window::Level::AlwaysOnTop,
            platform_specific: PlatformSpecific {
                skip_taskbar: true,
                ..Default::default()
            },
            ..Default::default()
        })
        .style(|_, _| theme::Style {
            background_color: Color::TRANSPARENT,
            text_color: Color::WHITE,
        })
        .subscription(Island::subscription)
        .run()
}

impl Island {
    fn boot() -> (Self, Task<Message>) {
        // Ask for the monitor size, then report back so update() can centre the pill.
        let find_window = window::oldest().and_then(|id| {
            window::monitor_size(id)
                .and_then(move |monitor| Task::done(Message::WindowReady(id, monitor.width)))
        });
        (Self::new(), find_window)
    }

    fn new() -> Self {
        let (time_text, until_next_minute) = read_clock();
        Self {
            time_text,
            until_next_minute,
            hovered: false,
            size: COLLAPSED,
            last_frame: None,
        }
    }

    fn goal(&self) -> Size {
        if self.hovered { EXPANDED } else { COLLAPSED }
    }

    fn animating(&self) -> bool {
        self.size != self.goal()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::MinuteTick => {
                (self.time_text, self.until_next_minute) = read_clock();
                Task::none()
            }
            // Hover only changes the goal; the frames do the moving.
            Message::Hovered(hovered) => {
                self.hovered = hovered;
                Task::none()
            }
            // The window is centred once, here; it never moves or resizes afterwards.
            Message::WindowReady(id, monitor_width) => {
                window::move_to(id, Point::new((monitor_width - WINDOW.width) / 2.0, TOP_MARGIN))
            }
            Message::Frame(at) => {
                if !self.animating() {
                    self.last_frame = None;
                    return Task::none();
                }
                // First frame of a movement has no previous frame to measure from.
                let dt = self
                    .last_frame
                    .map_or(1.0 / 60.0, |previous| at.duration_since(previous).as_secs_f32());
                self.last_frame = Some(at);
                self.size = step(self.size, self.goal(), dt);
                Task::none()
            }
        }
    }

    // view only describes what to draw from the current state; it never changes it.
    fn view(&self) -> Element<'_, Message> {
        // 0.0 when collapsed, 1.0 when fully expanded; scales the font with the pill.
        let progress = ((self.size.width - COLLAPSED.width) / (EXPANDED.width - COLLAPSED.width)).clamp(0.0, 1.0);
        let font_size = FONT_COLLAPSED + (FONT_EXPANDED - FONT_COLLAPSED) * progress;

        let pill = container(text(&self.time_text).size(font_size))
            .center_x(self.size.width)
            .center_y(self.size.height)
            .style(|_| container::Style {
                background: Some(Background::Color(Color::BLACK)),
                border: Border::default().rounded(self.size.height / 2.0),
                ..Default::default()
            });

        // Hover is detected on the pill only, not on the transparent window around it.
        let hoverable = mouse_area(pill)
            .on_enter(Message::Hovered(true))
            .on_exit(Message::Hovered(false));

        container(hoverable).center_x(Length::Fill).into()
    }

    // The minute timer always runs; the frame heartbeat only runs while the pill is moving.
    fn subscription(&self) -> Subscription<Message> {
        let minute = time::every(self.until_next_minute).map(|_| Message::MinuteTick);
        if self.animating() {
            Subscription::batch([minute, window::frames().map(Message::Frame)])
        } else {
            minute
        }
    }
}

// Reads the clock once; the text and the wait until the next minute both come from this reading.
fn read_clock() -> (String, Duration) {
    let now = Local::now();
    (
        now.format("%H:%M").to_string(),
        delay_until_next_minute(now.second(), now.nanosecond()),
    )
}

// Time left until the clock reads :00, plus a small margin so we wake inside the new minute.
fn delay_until_next_minute(second: u32, nanosecond: u32) -> Duration {
    let elapsed = Duration::new(second as u64, nanosecond);
    Duration::from_secs(60) - elapsed + Duration::from_millis(20)
}

// One animation frame: move `current` a fraction of the way to `goal`, snapping when close.
// The fraction depends on the elapsed time `dt`, so the speed is the same on any refresh rate.
fn step(current: Size, goal: Size, dt: f32) -> Size {
    let fraction = 1.0 - (-dt.clamp(0.0, 0.05) / SMOOTHING_SECONDS).exp();
    let towards = |from: f32, to: f32| {
        if (to - from).abs() < SNAP_DISTANCE { to } else { from + (to - from) * fraction }
    };
    Size::new(towards(current.width, goal.width), towards(current.height, goal.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_until_next_minute() {
        let margin = Duration::from_millis(20);
        assert_eq!(delay_until_next_minute(41, 0), Duration::from_secs(19) + margin);
        assert_eq!(delay_until_next_minute(0, 0), Duration::from_secs(60) + margin);
        assert_eq!(
            delay_until_next_minute(59, 500_000_000),
            Duration::from_millis(500) + margin
        );
    }

    #[test]
    fn step_moves_toward_goal_and_snaps() {
        let one = step(COLLAPSED, EXPANDED, 1.0 / 60.0);
        assert!(one.width > COLLAPSED.width && one.width < EXPANDED.width);

        // Repeated frames must arrive exactly, so the frame heartbeat can stop.
        let mut size = COLLAPSED;
        for _ in 0..120 {
            size = step(size, EXPANDED, 1.0 / 60.0);
        }
        assert_eq!(size, EXPANDED);

        // Reversing mid-way heads back from where it is, with no jump.
        let half = step(COLLAPSED, EXPANDED, 1.0 / 60.0);
        let back = step(half, COLLAPSED, 1.0 / 60.0);
        assert!(back.width < half.width && back.width >= COLLAPSED.width);
    }
}
