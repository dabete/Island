// Now-playing information from the Windows media sessions (the list Spotify, browsers and
// other players register with, which also feeds the volume flyout).
use std::cmp::Reverse;
use std::sync::Arc;
use std::time::Duration;

use iced::futures::Stream;
use iced::stream;

/// Covers are shrunk to fit inside this many pixels, so they cost about 37 KB, not the 1.6 MB
/// a full 640 x 640 cover takes once decoded.
const ART_SIZE: u32 = 96;

/// A decoded, shrunk cover: `rgba` holds width x height x 4 bytes. Shared, so cloning is cheap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Art {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

/// What the island shows about the music; the host decides how it looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlaying {
    pub app: String,
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub art: Option<Art>,
    /// Which buttons the player currently allows; some players refuse some actions.
    pub can_previous: bool,
    pub can_play_pause: bool,
    pub can_next: bool,
    /// Where in the track we were when this was read (the player's last report plus the time
    /// since, while playing), and how long the track is.
    pub position: Duration,
    pub duration: Duration,
    pub can_seek: bool,
}

/// What a button press or a drag asks the player to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Previous,
    PlayPause,
    Next,
    Seek(Duration),
}

/// Players may repeat their position every second or so. The pill already advances its own bar,
/// so a new reading only counts as a change when something else differs or the position is off
/// from where the old one predicted by more than this (a seek, a new track).
const POSITION_TOLERANCE: Duration = Duration::from_millis(1500);

/// Is `new` worth sending to the pill, given that `last` was sent `elapsed` ago?
pub fn changed(last: &Option<NowPlaying>, new: &Option<NowPlaying>, elapsed: Duration) -> bool {
    let (Some(last), Some(new)) = (last, new) else { return last != new };
    let expected = if last.playing { last.position + elapsed } else { last.position };
    let drift = expected.abs_diff(new.position);
    let same_position = NowPlaying { position: new.position, ..last.clone() };
    same_position != *new || drift > POSITION_TOLERANCE
}

/// Asks the winning session to do `command`. Returns at once; the player's reaction comes back
/// through `watch` like any other change.
pub fn send(command: Command) {    // shortcut: one short-lived thread per click (the Windows calls block), fine for human-speed clicks.
    #[cfg(windows)]
    std::thread::spawn(move || {
        let _ = windows_impl::send(command);
    });
    #[cfg(not(windows))]
    let _ = command;
}

/// How rounded the cover's corners are, as a share of its shorter side.
pub const CORNER_RADIUS: f32 = 0.14;

/// Decodes a PNG or JPEG cover, shrinks it (keeping its shape) to fit `ART_SIZE` and rounds its
/// corners. The rounding is baked into the pixels because iced's CPU renderer ignores the
/// border radius on images.
pub fn art_from_bytes(bytes: &[u8]) -> Option<Art> {
    let mut small = image::load_from_memory(bytes).ok()?.thumbnail(ART_SIZE, ART_SIZE).to_rgba8();
    round_corners(&mut small);
    Some(Art {
        width: small.width(),
        height: small.height(),
        rgba: small.into_raw().into(),
    })
}

// Fades the alpha of each pixel outside a quarter-circle at every corner (anti-aliased by one pixel).
fn round_corners(img: &mut image::RgbaImage) {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let radius = CORNER_RADIUS * w.min(h);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
        // Distance past the start of the nearest corner square, 0 when not in a corner.
        let dx = (radius - cx).max(cx - (w - radius)).max(0.0);
        let dy = (radius - cy).max(cy - (h - radius)).max(0.0);
        let coverage = (radius - dx.hypot(dy) + 0.5).clamp(0.0, 1.0);
        pixel.0[3] = (pixel.0[3] as f32 * coverage) as u8;
    }
}

/// What we need to know about one session to choose between several.
#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    pub playing: bool,
    pub spotify: bool,
    /// Windows' own idea of the current session.
    pub current: bool,
}

/// Playing beats paused, then Spotify beats other apps, then Windows' current session wins.
/// The first of any remaining ties wins.
pub fn pick(candidates: &[Candidate]) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .max_by_key(|(index, c)| (c.playing, c.spotify, c.current, Reverse(*index)))
        .map(|(index, _)| index)
}

/// A stream that yields the winning session whenever it changes (`None` = no session at all).
/// It is driven by Windows' own change events, never by polling.
pub fn watch() -> impl Stream<Item = Option<NowPlaying>> {
    stream::channel(8, async |sender| {
        #[cfg(windows)]
        std::thread::spawn(move || windows_impl::run(sender));
        #[cfg(not(windows))]
        let _ = sender;
        // Keeps the subscription alive; the listener thread does the work.
        std::future::pending::<()>().await;
    })
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use iced::futures::channel::mpsc::Sender;
    use windows::Foundation::TypedEventHandler;
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSession as Session,
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    };

    use windows::Storage::Streams::DataReader;

    use super::{Art, Candidate, Command, NowPlaying, art_from_bytes, changed, pick};

    // Windows raises its events on its own threads; we turn them into these signals.
    #[derive(Clone, Copy)]
    enum Signal {
        // The list of sessions changed, so event registrations must be redone.
        Resync,
        Refresh,
    }

    // A session we are listening to, with the tokens needed to stop listening.
    struct Registration {
        session: Session,
        properties_token: i64,
        playback_token: i64,
        timeline_token: i64,
    }

    pub fn run(mut sender: Sender<Option<NowPlaying>>) {
        // shortcut: if Windows refuses us a manager the island silently keeps showing the time.
        let _ = listen(&mut sender);
    }

    fn listen(sender: &mut Sender<Option<NowPlaying>>) -> windows::core::Result<()> {
        let manager = Manager::RequestAsync()?.join()?;
        let (tx, rx) = mpsc::channel::<Signal>();

        let signal = tx.clone();
        manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
            let _ = signal.send(Signal::Resync);
            Ok(())
        }))?;
        let signal = tx.clone();
        manager.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
            let _ = signal.send(Signal::Refresh);
            Ok(())
        }))?;

        let mut registered: Vec<Registration> = Vec::new();
        let mut resync = true;
        // What we last sent the pill, and when.
        let mut last: Option<(Option<NowPlaying>, Instant)> = None;
        // The cover of the current track, so pausing or resuming doesn't decode it again.
        let mut cover: Option<(String, Art)> = None;

        loop {
            if resync {
                for r in registered.drain(..) {
                    let _ = r.session.RemoveMediaPropertiesChanged(r.properties_token);
                    let _ = r.session.RemovePlaybackInfoChanged(r.playback_token);
                    let _ = r.session.RemoveTimelinePropertiesChanged(r.timeline_token);
                }
                for session in manager.GetSessions()? {
                    let signal = tx.clone();
                    let properties_token = session.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                        let _ = signal.send(Signal::Refresh);
                        Ok(())
                    }))?;
                    let signal = tx.clone();
                    let playback_token = session.PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                        let _ = signal.send(Signal::Refresh);
                        Ok(())
                    }))?;
                    let signal = tx.clone();
                    let timeline_token = session.TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
                        let _ = signal.send(Signal::Refresh);
                        Ok(())
                    }))?;
                    registered.push(Registration { session, properties_token, playback_token, timeline_token });
                }
                resync = false;
            }

            // A read can fail if a player closes mid-read; the next event reads again.
            let snapshot = read(&manager, &mut cover).unwrap_or(None);
            let worth_sending = match &last {
                None => true,
                Some((sent, at)) => changed(sent, &snapshot, at.elapsed()),
            };
            if worth_sending {
                match sender.try_send(snapshot.clone()) {
                    Ok(()) => last = Some((snapshot, Instant::now())),
                    Err(e) if e.is_disconnected() => return Ok(()),
                    // shortcut: a full queue drops this update, 8 slots is plenty for human-speed changes.
                    Err(_) => {}
                }
            }

            // Sleep until Windows reports a change, then wait for the burst to go quiet so
            // flicking through songs produces one update, not one per skipped track.
            let Ok(first) = rx.recv() else { return Ok(()) };
            resync |= matches!(first, Signal::Resync);
            while let Ok(next) = rx.recv_timeout(Duration::from_millis(60)) {
                resync |= matches!(next, Signal::Resync);
            }
        }
    }

    /// The session the island should show and control: (app id, is playing, the session).
    fn winning_session(manager: &Manager) -> windows::core::Result<Option<(String, bool, Session)>> {
        let current = manager
            .GetCurrentSession()
            .and_then(|s| s.SourceAppUserModelId())
            .map(|id| id.to_string())
            .ok();

        let mut sessions = Vec::new();
        let mut candidates = Vec::new();
        for session in manager.GetSessions()? {
            let app = session.SourceAppUserModelId()?.to_string();
            let playing = session.GetPlaybackInfo()?.PlaybackStatus()? == Status::Playing;
            candidates.push(Candidate {
                playing,
                spotify: app.to_lowercase().contains("spotify"),
                current: current.as_deref() == Some(app.as_str()),
            });
            sessions.push((app, playing, session));
        }

        Ok(pick(&candidates).map(|index| sessions.swap_remove(index)))
    }

    /// Does what a button press asked, on the same session the island is showing.
    pub fn send(command: Command) -> windows::core::Result<()> {
        let manager = Manager::RequestAsync()?.join()?;
        let Some((_, _, session)) = winning_session(&manager)? else { return Ok(()) };
        // The returned value says whether the player accepted; the change itself arrives as an event.
        match command {
            Command::Previous => session.TrySkipPreviousAsync()?.join()?,
            Command::PlayPause => session.TryTogglePlayPauseAsync()?.join()?,
            Command::Next => session.TrySkipNextAsync()?.join()?,
            // Windows counts time in units of 100 nanoseconds.
            Command::Seek(to) => session.TryChangePlaybackPositionAsync((to.as_nanos() / 100) as i64)?.join()?,
        };
        Ok(())
    }

    fn read(
        manager: &Manager,
        cover: &mut Option<(String, Art)>,
    ) -> windows::core::Result<Option<NowPlaying>> {
        let Some((app, playing, session)) = winning_session(manager)? else { return Ok(None) };
        let controls = session.GetPlaybackInfo()?.Controls()?;
        let properties = session.TryGetMediaPropertiesAsync()?.join()?;
        let title = properties.Title()?.to_string();
        let artist = properties.Artist()?.to_string();

        // Decode only when the track changed. A missing cover is retried on the next event,
        // because some players publish it a moment after the title.
        let track = format!("{app}\n{title}\n{artist}");
        if cover.as_ref().is_none_or(|(key, _)| *key != track) {
            *cover = read_cover(&properties).map(|art| (track.clone(), art));
        }

        // The player reports a position at some moment (`LastUpdatedTime`); while playing, the
        // real position is that plus the time since. Times are in 100 ns units.
        let timeline = session.GetTimelineProperties()?;
        let ticks = |ticks: i64| Duration::from_nanos(ticks.max(0) as u64 * 100);
        let start = ticks(timeline.StartTime()?.Duration);
        let duration = ticks(timeline.EndTime()?.Duration).saturating_sub(start);
        let mut position = ticks(timeline.Position()?.Duration).saturating_sub(start);
        if playing {
            // Windows' DateTime counts from 1601; the Unix epoch is this many 100 ns units later.
            const UNIX_EPOCH_IN_WINDOWS_TICKS: i128 = 116_444_736_000_000_000;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() / 100;
            let age = now as i128 + UNIX_EPOCH_IN_WINDOWS_TICKS - timeline.LastUpdatedTime()?.UniversalTime as i128;
            position += ticks(age.clamp(0, i64::MAX as i128) as i64);
        }

        Ok(Some(NowPlaying {
            app,
            title,
            artist,
            playing,
            art: cover.as_ref().map(|(_, art)| art.clone()),
            can_previous: controls.IsPreviousEnabled()?,
            can_play_pause: controls.IsPlayEnabled()?
                || controls.IsPauseEnabled()?
                || controls.IsPlayPauseToggleEnabled()?,
            can_next: controls.IsNextEnabled()?,
            position: position.min(duration),
            duration,
            can_seek: controls.IsPlaybackPositionEnabled()? && !duration.is_zero(),
        }))
    }

    fn read_cover(
        properties: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties,
    ) -> Option<Art> {
        let stream = properties.Thumbnail().ok()?.OpenReadAsync().ok()?.join().ok()?;
        let size = u32::try_from(stream.Size().ok()?).ok()?;
        let reader = DataReader::CreateDataReader(&stream).ok()?;
        reader.LoadAsync(size).ok()?.join().ok()?;
        let mut bytes = vec![0u8; size as usize];
        reader.ReadBytes(&mut bytes).ok()?;
        art_from_bytes(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(playing: bool, spotify: bool, current: bool) -> Candidate {
        Candidate { playing, spotify, current }
    }

    #[test]
    fn covers_are_shrunk_but_keep_their_shape() {
        // A 400 x 200 picture, encoded as a PNG the way Windows hands us covers.
        let source = image::RgbaImage::from_pixel(400, 200, image::Rgba([200, 30, 30, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let art = art_from_bytes(&png).unwrap();
        assert_eq!((art.width, art.height), (96, 48));
        assert_eq!(art.rgba.len(), 96 * 48 * 4);
        let alpha = |x: usize, y: usize| art.rgba[(y * 96 + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 0, "corner is cut away");
        assert_eq!(alpha(48, 24), 255, "middle is untouched");
        assert_eq!(alpha(48, 0), 255, "edges between the corners are untouched");
        assert!(art_from_bytes(b"not an image").is_none());
    }

    fn song(playing: bool, position_secs: u64) -> Option<NowPlaying> {
        Some(NowPlaying {
            app: "Spotify.exe".into(),
            title: "t".into(),
            artist: "a".into(),
            playing,
            art: None,
            can_previous: true,
            can_play_pause: true,
            can_next: true,
            position: Duration::from_secs(position_secs),
            duration: Duration::from_secs(200),
            can_seek: true,
        })
    }

    #[test]
    fn a_position_the_pill_would_have_reached_by_itself_is_not_news() {
        let secs = Duration::from_secs;
        // Playing: 46 s three seconds ago, so 49 s now is exactly what the pill's own bar shows.
        assert!(!changed(&song(true, 46), &song(true, 49), secs(3)));
        // A seek (a big jump), a pause, and a new track all count.
        assert!(changed(&song(true, 46), &song(true, 120), secs(3)));
        assert!(changed(&song(true, 46), &song(false, 49), secs(3)));
        // Paused: the position must not move at all (apart from the tolerance).
        assert!(changed(&song(false, 46), &song(false, 60), secs(3)));
        assert!(!changed(&song(false, 46), &song(false, 46), secs(30)));
        // Appearing or disappearing is always news.
        assert!(changed(&None, &song(true, 0), secs(1)));
        assert!(changed(&song(true, 0), &None, secs(1)));
    }

    #[test]
    fn no_sessions_means_nothing_to_show() {
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn playing_beats_paused_even_for_spotify() {
        let sessions = [c(false, true, true), c(true, false, false)];
        assert_eq!(pick(&sessions), Some(1));
    }

    #[test]
    fn spotify_wins_when_both_play() {
        let sessions = [c(true, false, true), c(true, true, false)];
        assert_eq!(pick(&sessions), Some(1));
    }

    #[test]
    fn paused_sessions_prefer_spotify_then_windows_current() {
        assert_eq!(pick(&[c(false, false, true), c(false, true, false)]), Some(1));
        assert_eq!(pick(&[c(false, false, false), c(false, false, true)]), Some(1));
    }

    #[test]
    fn first_session_wins_a_complete_tie() {
        assert_eq!(pick(&[c(true, false, false), c(true, false, false)]), Some(0));
    }
}
