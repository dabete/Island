// Release builds run without a console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod fullscreen;
mod media;
mod platform;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Local, Timelike};
use iced::widget::image as picture;
use iced::widget::image::Handle;
use iced::widget::scrollable::{AbsoluteOffset, Viewport};
use iced::widget::{
    Id, button, column, container, mouse_area, operation, pin, row, scrollable, slider, space, stack, text,
};
use iced::window::{self, settings::PlatformSpecific};
use iced::{
    Alignment, Background, Border, Color, Element, Font, Length, Point, Size, Subscription, Task, theme, time,
};

const COLLAPSED: Size = Size::new(110.0, 34.0);
// Collapsed while music plays: wider, to fit the cover and the equaliser.
const COLLAPSED_MUSIC: Size = Size::new(170.0, 34.0);
const EXPANDED: Size = Size::new(200.0, 64.0);
// Expanded while a media session exists: room for the cover, the text and the controls.
const EXPANDED_MUSIC: Size = Size::new(360.0, 140.0);
// The decorative equaliser redraws this often, and only while music plays.
const EQUALISER_INTERVAL: Duration = Duration::from_millis(66);
// The OS window never changes size: it is big enough for the largest pill plus a margin,
// and the pill is drawn inside it. Resizing the window every frame flickered.
// It sits at the very top of the screen, with the pill TOP_MARGIN below the top edge inside it,
// so the pill can slide up out of the screen when an app goes fullscreen.
const WINDOW: Size = Size::new(380.0, 164.0);
// Time constant of the slide off the top of the screen (about 4 x this to finish, ~500 ms).
const AWAY_SECONDS: f32 = 0.12;
const FONT_COLLAPSED: f32 = 16.0;
const FONT_EXPANDED: f32 = 30.0;
const TOP_MARGIN: f32 = 8.0;
// Time constant of the slow-down: bigger = slower. About 4 x this to settle (~350 ms).
const SMOOTHING_SECONDS: f32 = 0.08;
// Closer than this (in pixels) to the goal counts as arrived.
const SNAP_DISTANCE: f32 = 0.5;
// The crossfade between the time and the music view settles in about 4 x this (~700 ms), slower
// than the size animation so the fade can be watched.
const MIX_SECONDS: f32 = 0.18;
const MIX_SNAP: f32 = 0.01;
// A title wider than its column slides sideways: wait, slide at a steady speed, wait, jump back.
const MARQUEE_WAIT_START: f32 = 2.0;
const MARQUEE_WAIT_END: f32 = 1.0;
// Logical pixels per second.
const MARQUEE_SPEED: f32 = 40.0;
// 24 steps a second while sliding (about 1.7 px each); nothing at all while waiting.
const MARQUEE_INTERVAL: Duration = Duration::from_millis(42);
const OVERFLOW_TOLERANCE: f32 = 2.0;
const TITLE_ID: &str = "title";
const ARTIST_ID: &str = "artist";

// State: everything the app must remember between events.
struct Island {
    time_text: String,
    until_next_minute: Duration,
    // Is the mouse over the pill? The pill counts as hovered (see `hovered()`) while this is true
    // or while a drag is in progress, so it doesn't collapse under your hand.
    pointer_inside: bool,
    // While dragging the progress bar: where the handle is, in seconds. Windows is not told until release.
    drag: Option<f32>,
    // The track position Windows last gave us, when it arrived, and the latest tick (the view
    // computes "position now" from these without reading the clock itself).
    position: Duration,
    position_at: Instant,
    now: Instant,
    // Current drawn size of the pill; the goal is derived from `hovered()`.
    size: Size,
    last_frame: Option<Instant>,
    window: Option<window::Id>,
    // The rectangle Windows currently treats as clickable (see platform.rs): width, top, bottom.
    clickable: Option<[f32; 3]>,
    // Is another app fullscreen on the main monitor (see fullscreen.rs)? Then the pill slides up
    // off the screen: `away` goes from 0.0 (shown) to 1.0 (gone), and the window is then hidden.
    fullscreen: bool,
    away: f32,
    window_hidden: bool,
    // The winning media session, if any (see media.rs).
    now_playing: Option<media::NowPlaying>,
    // The cover as iced draws it; built once per cover, because a new handle means a new upload.
    art: Option<Handle>,
    equaliser_tick: u32,
    // 0.0 = showing the time, 1.0 = showing the music, in between = mid-fade (collapsed pill only).
    mix: f32,
    // How far the title and the artist stick out past their column (0 = they fit), reported by iced.
    title_overflow: f32,
    artist_overflow: f32,
    // When the current marquee cycle began, and how far each line is currently slid.
    marquee_origin: Option<Instant>,
    marquee_offsets: [f32; 2],
}

// Messages: the things that can happen.
#[derive(Debug, Clone)]
enum Message {
    MinuteTick,
    Hovered(bool),
    Frame(Instant),
    NowPlaying(Option<media::NowPlaying>),
    Fullscreen(bool),
    EqualiserTick,
    ProgressTick(Instant),
    SeekDragged(f32),
    SeekReleased,
    MarqueeTick(Instant),
    TitleViewport(Viewport),
    ArtistViewport(Viewport),
    // A button press: update asks Windows to do it, and the result comes back as NowPlaying.
    Control(media::Command),
    // Window id and monitor width, known once the window exists.
    WindowReady(window::Id, f32),
}

fn main() -> iced::Result {
    iced::application(Island::boot, Island::update, Island::view)
        .window(window::Settings {
            size: WINDOW,
            // Real position is set once the monitor width is known.
            position: window::Position::Specific(Point::new(0.0, 0.0)),
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
            pointer_inside: false,
            drag: None,
            position: Duration::ZERO,
            position_at: Instant::now(),
            now: Instant::now(),
            size: COLLAPSED,
            last_frame: None,
            window: None,
            clickable: None,
            fullscreen: false,
            away: 0.0,
            window_hidden: false,
            now_playing: None,
            art: None,
            equaliser_tick: 0,
            mix: 0.0,
            title_overflow: 0.0,
            artist_overflow: 0.0,
            marquee_origin: None,
            marquee_offsets: [0.0; 2],
        }
    }

    fn mix_target(&self) -> f32 {
        if self.is_playing() { 1.0 } else { 0.0 }
    }

    fn fading(&self) -> bool {
        self.mix != self.mix_target()
    }

    // Something is overflowing its column in the expanded view, so the marquee needs its ticks.
    fn marquee_running(&self) -> bool {
        self.hovered() && self.now_playing.is_some() && (self.title_overflow > 0.0 || self.artist_overflow > 0.0)
    }

    // The pill stays expanded while you drag the progress bar, even if the mouse leaves it.
    // While another app is fullscreen it is never expanded.
    fn hovered(&self) -> bool {
        !self.fullscreen && (self.pointer_inside || self.drag.is_some())
    }

    fn away_target(&self) -> f32 {
        if self.fullscreen { 1.0 } else { 0.0 }
    }

    fn sliding(&self) -> bool {
        self.away != self.away_target()
    }

    // Fully off the top of the screen: nothing needs to tick or draw until it comes back.
    fn off_screen(&self) -> bool {
        self.fullscreen && self.away >= 1.0
    }

    // Where in the track we are now, in seconds: the handle while dragging, otherwise the
    // position Windows gave us plus the time since (while playing).
    fn shown_position(&self, song: &media::NowPlaying) -> f32 {
        if let Some(dragged) = self.drag {
            return dragged;
        }
        let advanced = if song.playing { self.now.saturating_duration_since(self.position_at) } else { Duration::ZERO };
        (self.position + advanced).min(song.duration).as_secs_f32()
    }

    // The cycle starts the moment iced first says something is too wide. It cannot wait for the
    // first tick, because during the opening wait there are no ticks.
    fn start_marquee_clock(&mut self) {
        if self.marquee_origin.is_none() && (self.title_overflow > 0.0 || self.artist_overflow > 0.0) {
            self.marquee_origin = Some(Instant::now());
        }
    }

    // Back to the start: nothing slid, overflow unknown until iced reports it again.
    fn reset_marquee(&mut self) -> Task<Message> {
        self.title_overflow = 0.0;
        self.artist_overflow = 0.0;
        self.marquee_origin = None;
        self.marquee_offsets = [0.0; 2];
        Task::batch([TITLE_ID, ARTIST_ID].map(|id| {
            operation::scroll_to(Id::new(id), AbsoluteOffset { x: Some(0.0), y: None })
        }))
    }

    fn is_playing(&self) -> bool {
        self.now_playing.as_ref().is_some_and(|song| song.playing)
    }

    // The size the pill is heading for follows what it is showing, and whether it is hovered.
    fn goal(&self) -> Size {
        match (self.hovered(), self.is_playing(), self.now_playing.is_some()) {
            (true, _, true) => EXPANDED_MUSIC,
            (true, _, false) => EXPANDED,
            (false, true, _) => COLLAPSED_MUSIC,
            (false, false, _) => COLLAPSED,
        }
    }

    fn animating(&self) -> bool {
        self.size != self.goal()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.handle(message);
        Task::batch([task, self.sync_clickable_area(), self.sync_window_visibility()])
    }

    // While the pill is moving the clickable area covers both its current and its goal size, so it
    // never clips the pill; it shrinks to the pill's size only once everything has settled. While the
    // pill is off its resting place (sliding up or down) the area also reaches the top of the screen.
    fn sync_clickable_area(&mut self) -> Task<Message> {
        let goal = self.goal();
        let area = if self.animating() {
            Size::new(self.size.width.max(goal.width), self.size.height.max(goal.height))
        } else {
            self.size
        };
        let top = if self.away > 0.0 { 0.0 } else { TOP_MARGIN };
        let wanted = [area.width, top, TOP_MARGIN + area.height];
        let Some(id) = self.window else { return Task::none() };
        if self.clickable == Some(wanted) {
            return Task::none();
        }
        self.clickable = Some(wanted);
        window::run(id, move |window| {
            platform::set_clickable_area(window, WINDOW, wanted[0], wanted[1], wanted[2])
        })
        .discard()
    }

    // The window is really hidden once the pill is off the screen, and shown again (without taking
    // keyboard focus) the moment the fullscreen app is gone. Hidden, it cannot block clicks and
    // does not make Windows compose a fullscreen game the slow way.
    fn sync_window_visibility(&mut self) -> Task<Message> {
        let hidden = self.off_screen();
        let Some(id) = self.window else { return Task::none() };
        if hidden == self.window_hidden {
            return Task::none();
        }
        self.window_hidden = hidden;
        window::run(id, move |window| platform::set_visible(window, !hidden)).discard()
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::MinuteTick => {
                (self.time_text, self.until_next_minute) = read_clock();
                Task::none()
            }
            Message::NowPlaying(now_playing) => {
                // Pausing or resuming resends the same cover; only a different one needs a new handle.
                let old = self.now_playing.as_ref().and_then(|song| song.art.as_ref());
                let new = now_playing.as_ref().and_then(|song| song.art.as_ref());
                let same_cover = match (old, new) {
                    (Some(a), Some(b)) => Arc::ptr_eq(&a.rgba, &b.rgba),
                    (None, None) => true,
                    _ => false,
                };
                if !same_cover {
                    self.art = new.map(|art| Handle::from_rgba(art.width, art.height, art.rgba.to_vec()));
                }
                // A different title or artist may fit differently, so the marquee starts over.
                let text_of = |song: &media::NowPlaying| (song.title.clone(), song.artist.clone());
                let text_changed = self.now_playing.as_ref().map(text_of) != now_playing.as_ref().map(text_of);
                // A new position from Windows restarts the bar's own counting (not mid-drag).
                if let (Some(song), None) = (&now_playing, self.drag) {
                    self.position = song.position;
                    self.position_at = Instant::now();
                    self.now = self.position_at;
                }
                self.now_playing = now_playing;
                if text_changed { self.reset_marquee() } else { Task::none() }
            }
            Message::TitleViewport(viewport) => {
                self.title_overflow = overflow(&viewport);
                self.start_marquee_clock();
                Task::none()
            }
            Message::ArtistViewport(viewport) => {
                self.artist_overflow = overflow(&viewport);
                self.start_marquee_clock();
                Task::none()
            }
            Message::MarqueeTick(at) => {
                let t = at.duration_since(*self.marquee_origin.get_or_insert(at)).as_secs_f32();                let lines = [(TITLE_ID, self.title_overflow), (ARTIST_ID, self.artist_overflow)];
                let mut tasks = Vec::new();
                for (index, (id, overflow)) in lines.into_iter().enumerate() {
                    if overflow <= 0.0 {
                        continue;
                    }
                    let offset = marquee_offset(t, overflow);
                    // Only talk to iced when the line actually moved, so the waits cost nothing.
                    if (offset - self.marquee_offsets[index]).abs() >= 0.25 {                        self.marquee_offsets[index] = offset;
                        tasks.push(operation::scroll_to(Id::new(id), AbsoluteOffset { x: Some(offset), y: None }));
                    }
                }
                Task::batch(tasks)
            }
            Message::Fullscreen(fullscreen) => {
                self.fullscreen = fullscreen;
                if fullscreen {
                    // The mouse can't have been over a pill that is leaving; forget it.
                    self.pointer_inside = false;
                    self.drag = None;
                }
                Task::none()
            }
            Message::Control(command) => {
                media::send(command);
                Task::none()
            }
            Message::EqualiserTick => {
                self.equaliser_tick = self.equaliser_tick.wrapping_add(1);
                Task::none()
            }
            // Hover only changes the goal; the frames do the moving.
            Message::Hovered(inside) => {
                let was_hovered = self.hovered();
                self.pointer_inside = inside;
                if inside {
                    // A fresh look at the position, so the bar starts accurate.
                    self.now = Instant::now();
                }
                if self.hovered() && !was_hovered { self.reset_marquee() } else { Task::none() }
            }
            Message::ProgressTick(at) => {
                self.now = at;
                Task::none()
            }
            Message::SeekDragged(seconds) => {
                self.drag = Some(seconds);
                Task::none()
            }
            // Only now is Windows asked to move; until then the handle was ours alone.
            Message::SeekReleased => {
                if let Some(seconds) = self.drag.take() {
                    let to = Duration::from_secs_f32(seconds.max(0.0));
                    media::send(media::Command::Seek(to));
                    // Show the new spot at once; Windows' confirmation will agree with it.
                    self.position = to;
                    self.position_at = Instant::now();
                    self.now = self.position_at;
                }
                Task::none()
            }
            // The window is centred once, here; it never moves or resizes afterwards.
            Message::WindowReady(id, monitor_width) => {
                self.window = Some(id);
                window::move_to(id, Point::new((monitor_width - WINDOW.width) / 2.0, 0.0))
            }
            Message::Frame(at) => {
                if !self.animating() && !self.fading() && !self.sliding() {
                    self.last_frame = None;
                    return Task::none();
                }
                // First frame of a movement has no previous frame to measure from.
                let dt = self
                    .last_frame
                    .map_or(1.0 / 60.0, |previous| at.duration_since(previous).as_secs_f32());
                self.last_frame = Some(at);
                self.size = step(self.size, self.goal(), dt);
                self.mix = approach(self.mix, self.mix_target(), dt, MIX_SECONDS, MIX_SNAP);
                self.away = approach(self.away, self.away_target(), dt, AWAY_SECONDS, MIX_SNAP);
                Task::none()
            }
        }
    }

    // view only describes what to draw from the current state; it never changes it.
    fn view(&self) -> Element<'_, Message> {
        // 0.0 when collapsed, 1.0 when fully expanded; scales the time's font with the pill's height.
        let progress = ((self.size.height - COLLAPSED.height) / (EXPANDED.height - COLLAPSED.height)).clamp(0.0, 1.0);
        let font_size = FONT_COLLAPSED + (FONT_EXPANDED - FONT_COLLAPSED) * progress;

        let content: Element<Message> = match (&self.now_playing, self.hovered()) {
            (Some(song), true) => self.expanded_music(song),
            (None, true) => text(&self.time_text).size(font_size).into(),
            (_, false) => self.collapsed_content(font_size),
        };

        let pill = container(content)
            .center_x(self.size.width)
            .center_y(self.size.height)
            .clip(true)
            .style(|_| container::Style {
                background: Some(Background::Color(Color::BLACK)),
                border: Border::default().rounded(self.size.height / 2.0),
                ..Default::default()
            });

        // Hover is detected on the pill only, not on the transparent window around it.
        let hoverable = mouse_area(pill)
            .on_enter(Message::Hovered(true))
            .on_exit(Message::Hovered(false));

        // The pill rests TOP_MARGIN below the top of the window; `away` slides it up and out of it.
        let top = TOP_MARGIN - self.away * (self.size.height + TOP_MARGIN);
        pin(hoverable).x((WINDOW.width - self.size.width) / 2.0).y(top).into()
    }

    // The cover (its corners are already rounded in the pixels), or a grey rounded square while
    // there is none. `opacity` 1.0 is solid; less lets what is behind it show through.
    fn cover(&self, side: f32, opacity: f32) -> Element<'static, Message> {
        match &self.art {
            Some(handle) => picture(handle.clone()).width(side).height(side).opacity(opacity).into(),
            None => container(space::horizontal())
                .width(side)
                .height(side)
                .style(move |_| container::Style {
                    background: Some(Background::Color(Color { a: opacity, ..Color::from_rgb8(60, 60, 60) })),
                    border: Border::default().rounded(media::CORNER_RADIUS * side),
                    ..Default::default()
                })
                .into(),
        }
    }

    // Cover on the left, decorative equaliser on the right.
    fn compact_music(&self, opacity: f32) -> Element<'_, Message> {
        row![self.cover(24.0, opacity), space::horizontal(), equaliser(self.equaliser_tick, opacity)]
            .align_y(Alignment::Center)
            .padding([0, 12])
            .into()
    }

    // The collapsed pill: the time and the compact music view, cross-faded by `mix`.
    fn collapsed_content(&self, font_size: f32) -> Element<'_, Message> {
        let time = container(text(&self.time_text).size(font_size).color(Color { a: 1.0 - self.mix, ..Color::WHITE }))
            .center(Length::Fill);
        if self.mix <= 0.0 {
            time.into()
        } else if self.mix >= 1.0 {
            self.compact_music(1.0)
        } else {
            stack![time, self.compact_music(self.mix)].into()
        }
    }

    // Cover on the left; on the right the title, the artist and the three buttons.
    // Step 2c-2 adds the progress bar between the artist and the buttons.
    fn expanded_music<'a>(&'a self, song: &'a media::NowPlaying) -> Element<'a, Message> {
        use media::Command;
        let play_pause = if song.playing { "\u{23F8}" } else { "\u{25B6}" };
        let controls = row![
            control("\u{23EE}", song.can_previous.then_some(Command::Previous)),
            control(play_pause, song.can_play_pause.then_some(Command::PlayPause)),
            control("\u{23ED}", song.can_next.then_some(Command::Next)),
        ]
        .spacing(18)
        .align_y(Alignment::Center);

        // A title or artist wider than this column slides sideways (see MarqueeTick). The scrollable
        // hides its bar, clips at the column's edge, and tells us how far the text sticks out.
        let line = |content: iced::widget::Text<'a>, id: &'static str, on_view: fn(Viewport) -> Message| {
            scrollable(content.wrapping(text::Wrapping::None))
                .id(Id::new(id))
                .direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::hidden()))
                .on_scroll(on_view)
                .width(Length::Fill)
        };
        let details = column![
            line(text(&song.title).size(17), TITLE_ID, Message::TitleViewport),
            line(
                text(&song.artist).size(13).color(Color::from_rgb8(160, 160, 160)),
                ARTIST_ID,
                Message::ArtistViewport
            ),
            space::vertical(),
        ]
        .extend(song.can_seek.then(|| self.progress_bar(song)))
        .push(container(controls).center_x(Length::Fill))
        .spacing(4)
        .width(Length::Fill)
        .height(Length::Fill);

        // The pill's ends are fully round, so the sides need more room than the top and bottom.
        row![self.cover(88.0, 1.0), details]
            .spacing(18)
            .padding([16, 32])
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Alignment::Center)
            .into()
    }

    // Elapsed time, a draggable bar, total time. Dragging only moves the handle (SeekDragged);
    // Windows is asked to jump when you let go (SeekReleased).
    fn progress_bar(&self, song: &media::NowPlaying) -> Element<'_, Message> {
        let total = song.duration.as_secs_f32();
        let here = self.shown_position(song);
        let label = |seconds: f32| text(clock_text(seconds)).size(11).color(Color::from_rgb8(160, 160, 160));
        row![
            label(here),
            slider(0.0..=total, here, Message::SeekDragged)
                .step(0.5_f32)
                .on_release(Message::SeekReleased)
                .height(14)
                .style(progress_style),
            label(total),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    }

    // The minute timer always runs; the frame heartbeat only runs while the pill is moving, and the
    // equaliser's only while music plays in the collapsed pill. Otherwise the app is idle.
    fn subscription(&self) -> Subscription<Message> {
        let minute = time::every(self.until_next_minute).map(|_| Message::MinuteTick);
        // Windows tells us when the music changes; this sleeps in between.
        let music = Subscription::run(media::watch).map(Message::NowPlaying);
        // Windows tells us when an app goes fullscreen or leaves it; this sleeps in between too.
        let fullscreen = Subscription::run(fullscreen::watch).map(Message::Fullscreen);
        let mut subscriptions = vec![minute, music, fullscreen];
        if self.animating() || self.fading() || self.sliding() {
            subscriptions.push(window::frames().map(Message::Frame));
        }
        // Off the top of the screen nobody can see the equaliser, so it stops too.
        if self.is_playing() && !self.hovered() && !self.off_screen() {
            subscriptions.push(time::every(EQUALISER_INTERVAL).map(|_| Message::EqualiserTick));
        }
        // The progress bar moves once a second, and only while you can see it moving.
        if self.hovered() && self.is_playing() && self.drag.is_none() {
            subscriptions.push(time::every(Duration::from_secs(1)).map(Message::ProgressTick));
        }
        if self.marquee_running() {
            // Tick steadily while a line is sliding; while every line is waiting, sleep until the
            // moment the first of them starts or finishes waiting (a single wake-up).
            let t = self.marquee_origin.map_or(0.0, |origin| origin.elapsed().as_secs_f32());
            let mut sliding = false;
            let mut wake: Option<Duration> = None;
            for overflow in [self.title_overflow, self.artist_overflow].into_iter().filter(|o| *o > 0.0) {
                match marquee_phase(t, overflow) {
                    MarqueePhase::Sliding => sliding = true,
                    MarqueePhase::Waiting(until) => wake = Some(wake.map_or(until, |w| w.min(until))),
                }
            }
            if sliding {
                subscriptions.push(time::every(MARQUEE_INTERVAL).map(Message::MarqueeTick));
            } else if let Some(until) = wake {
                // A few milliseconds past the boundary, so the wake-up never lands just before it.
                subscriptions.push(time::every(until + Duration::from_millis(10)).map(Message::MarqueeTick));
            }
        }
        Subscription::batch(subscriptions)
    }
}

// A thin white bar with a small round handle (the default blue would clash with the black pill).
fn progress_style(_theme: &iced::Theme, status: slider::Status) -> slider::Style {
    let grabbed = matches!(status, slider::Status::Hovered | slider::Status::Dragged);
    slider::Style {
        rail: slider::Rail {
            backgrounds: (Background::Color(Color::WHITE), Background::Color(Color::from_rgb8(70, 70, 70))),
            width: 3.0,
            border: Border::default().rounded(1.5),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle { radius: if grabbed { 6.0 } else { 4.5 } },
            background: Background::Color(Color::WHITE),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

// 3.5 seconds becomes "0:03", 207 seconds "3:27".
fn clock_text(seconds: f32) -> String {
    let whole = seconds.max(0.0) as u32;
    format!("{}:{:02}", whole / 60, whole % 60)
}

// A plain text button; with no command it is dimmed and does nothing (the player refuses that action).
fn control(glyph: &'static str, command: Option<media::Command>) -> Element<'static, Message> {
    // Segoe UI Symbol has plain, single-colour versions of these glyphs; without naming it Windows
    // falls back to the colour emoji font and draws blue squares.
    button(text(glyph).size(22).font(Font::with_name("Segoe UI Symbol")))
        .padding([2, 6])
        .on_press_maybe(command.map(Message::Control))
        .style(|_, status| button::Style {
            text_color: match status {
                button::Status::Disabled => Color::from_rgb8(90, 90, 90),
                button::Status::Hovered => Color::WHITE,
                _ => Color::from_rgb8(215, 215, 215),
            },
            ..button::Style::default()
        })
        .into()
}

// Four bars that bounce. They are decoration: Windows gives us no audio levels, so the heights
// come from the tick number alone.
fn equaliser(tick: u32, opacity: f32) -> Element<'static, Message> {
    let bars = (0..4).map(|bar| {
        let t = tick as f32 * 0.5 + bar as f32 * 1.7;
        let height = 4.0 + 12.0 * (t.sin() * (t * 0.37).cos()).abs();
        container(space::vertical())
            .width(3)
            .height(height)
            .style(move |_| container::Style {
                background: Some(Background::Color(Color { a: opacity, ..Color::WHITE })),
                border: Border::default().rounded(1.5),
                ..Default::default()
            })
            .into()
    });
    row(bars).spacing(2).align_y(Alignment::Center).into()
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
// The fraction depends on the elapsed time `dt`, so the speed is the same on any refresh rate;
// `tau` is the time constant (bigger = slower).
fn approach(current: f32, goal: f32, dt: f32, tau: f32, snap: f32) -> f32 {
    if (goal - current).abs() < snap {
        return goal;
    }
    let fraction = 1.0 - (-dt.clamp(0.0, 0.05) / tau).exp();
    current + (goal - current) * fraction
}

fn step(current: Size, goal: Size, dt: f32) -> Size {
    Size::new(
        approach(current.width, goal.width, dt, SMOOTHING_SECONDS, SNAP_DISTANCE),
        approach(current.height, goal.height, dt, SMOOTHING_SECONDS, SNAP_DISTANCE),
    )
}

// How far the text sticks out past its column, as reported by the scrollable.
fn overflow(viewport: &Viewport) -> f32 {
    let overflow = viewport.content_bounds().width - viewport.bounds().width;
    // Sub-pixel rounding makes short text report a fraction of a pixel; that is not worth sliding.
    if overflow < OVERFLOW_TOLERANCE { 0.0 } else { overflow }
}

#[derive(Debug, PartialEq)]
enum MarqueePhase {
    // Standing still; this long until it starts sliding or jumps back.
    Waiting(Duration),
    Sliding,
}

// Which part of its cycle a line `overflow` pixels too wide is in, `t` seconds after the cycle began.
fn marquee_phase(t: f32, overflow: f32) -> MarqueePhase {
    let slide = overflow / MARQUEE_SPEED;
    let cycle = MARQUEE_WAIT_START + slide + MARQUEE_WAIT_END;
    let t = t % cycle;
    if t < MARQUEE_WAIT_START {
        MarqueePhase::Waiting(Duration::from_secs_f32(MARQUEE_WAIT_START - t))
    } else if t < MARQUEE_WAIT_START + slide {
        MarqueePhase::Sliding
    } else {
        MarqueePhase::Waiting(Duration::from_secs_f32(cycle - t))
    }
}

// How far a line `overflow` pixels too wide has slid `t` seconds into its cycle: it waits, slides
// at a steady speed until its end shows, waits again, then jumps back and repeats.
fn marquee_offset(t: f32, overflow: f32) -> f32 {
    let slide = overflow / MARQUEE_SPEED;
    let t = t % (MARQUEE_WAIT_START + slide + MARQUEE_WAIT_END);
    if t < MARQUEE_WAIT_START { 0.0 } else { ((t - MARQUEE_WAIT_START) * MARQUEE_SPEED).min(overflow) }
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
    fn marquee_only_asks_for_ticks_while_sliding() {
        let overflow = 80.0; // slides for 2 s
        let waiting = |secs: f32| MarqueePhase::Waiting(Duration::from_secs_f32(secs));
        // The opening wait: asleep until it ends, no ticks.
        assert_eq!(marquee_phase(0.5, overflow), waiting(MARQUEE_WAIT_START - 0.5));
        // The slide: ticks.
        assert_eq!(marquee_phase(MARQUEE_WAIT_START + 0.1, overflow), MarqueePhase::Sliding);
        assert_eq!(marquee_phase(MARQUEE_WAIT_START + 1.9, overflow), MarqueePhase::Sliding);
        // The closing wait: asleep until the jump back.
        let cycle = MARQUEE_WAIT_START + 2.0 + MARQUEE_WAIT_END;
        assert_eq!(marquee_phase(MARQUEE_WAIT_START + 2.5, overflow), waiting(cycle - (MARQUEE_WAIT_START + 2.5)));
        // And the next cycle begins with the opening wait again.
        assert_eq!(marquee_phase(cycle + 0.5, overflow), waiting(MARQUEE_WAIT_START - 0.5));
    }

    #[test]
    fn times_read_as_minutes_and_seconds() {
        assert_eq!(clock_text(0.0), "0:00");
        assert_eq!(clock_text(3.9), "0:03");
        assert_eq!(clock_text(207.0), "3:27");
        assert_eq!(clock_text(-5.0), "0:00");
    }

    #[test]
    fn the_fade_takes_a_visible_while_and_ends_exactly() {
        let frame = 1.0 / 60.0;
        let (mut mix, mut frames) = (0.0, 0);
        while mix != 1.0 {
            let next = approach(mix, 1.0, frame, MIX_SECONDS, MIX_SNAP);
            assert!(next > mix, "fades forward only");
            mix = next;
            frames += 1;
        }
        // Between a third of a second and a second at 60 frames a second.
        assert!((20..60).contains(&frames), "took {frames} frames");
    }

    #[test]
    fn marquee_waits_slides_waits_and_repeats() {
        let overflow = 80.0; // 2 s of sliding at 40 px/s
        assert_eq!(marquee_offset(0.0, overflow), 0.0);
        assert_eq!(marquee_offset(MARQUEE_WAIT_START - 0.1, overflow), 0.0);
        assert_eq!(marquee_offset(MARQUEE_WAIT_START + 1.0, overflow), 40.0);
        assert_eq!(marquee_offset(MARQUEE_WAIT_START + 2.0 + 0.5, overflow), 80.0);
        // One full cycle later it is back at the start.
        let cycle = MARQUEE_WAIT_START + 2.0 + MARQUEE_WAIT_END;
        assert_eq!(marquee_offset(cycle + 0.5, overflow), 0.0);
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
