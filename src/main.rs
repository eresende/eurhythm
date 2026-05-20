use clap::Parser;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    style::{Color, ResetColor, SetForegroundColor, SetBackgroundColor},
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size,
    },
};
use rodio::{Decoder, Player, Source};
use rustfft::{FftPlanner, num_complex::Complex};
use std::{
    error::Error,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, TryRecvError},
    thread,
    time::Duration,
};

const SPECTRUM_BANDS: usize = 48;
const SPECTRUM_FPS: u32 = 24;
const FFT_SIZE: usize = 2048;
const BAR_RISE_EASING: f32 = 0.55;
const BAR_FALL_EASING: f32 = 0.18;
const PEAK_FALL_SPEED: f32 = 0.22;
const METER_BAR_WIDTH: usize = 2;
const METER_BAR_GAP: usize = 1;
const METER_HEIGHT: usize = 10;
const SEEK_STEP_SECONDS: i64 = 5;
const VOLUME_STEP: f32 = 0.05;
const MAX_VOLUME: f32 = 2.0;
const SPECTRUM_PREVIEW_SECONDS: u64 = 20;
const ANALYSIS_RESERVED_THREADS: usize = 2;
const BEGIN_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";
const MOVE_CURSOR_HOME: &[u8] = b"\x1b[H";
const CLEAR_FROM_CURSOR_DOWN: &[u8] = b"\x1b[J";
const PLAYLIST_MIN_ROWS: usize = 3;
const PLAYLIST_MAX_ROWS: usize = 8;
const NON_PLAYLIST_ROWS: usize = 24;

#[derive(Parser)]
#[command(
    author = "Eusebio Resende <me@eusebioresende.com>",
    about = "A simple music player written in Rust"
)]
struct Args {
    directory: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    fn next(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum VisualizerTheme {
    Cyberpunk,
    Sunset,
    Aurora,
    Classic,
}

impl VisualizerTheme {
    fn next(self) -> Self {
        match self {
            VisualizerTheme::Cyberpunk => VisualizerTheme::Sunset,
            VisualizerTheme::Sunset => VisualizerTheme::Aurora,
            VisualizerTheme::Aurora => VisualizerTheme::Classic,
            VisualizerTheme::Classic => VisualizerTheme::Cyberpunk,
        }
    }

    fn name(self) -> &'static str {
        match self {
            VisualizerTheme::Cyberpunk => "Cyberpunk Neon",
            VisualizerTheme::Sunset => "Golden Sunset",
            VisualizerTheme::Aurora => "Northern Aurora",
            VisualizerTheme::Classic => "Classic Vintage",
        }
    }
}

fn interpolate_color(c1: (u8, u8, u8), c2: (u8, u8, u8), factor: f32) -> Color {
    let factor = factor.clamp(0.0, 1.0);
    let r = (c1.0 as f32 + (c2.0 as f32 - c1.0 as f32) * factor).round() as u8;
    let g = (c1.1 as f32 + (c2.1 as f32 - c1.1 as f32) * factor).round() as u8;
    let b = (c1.2 as f32 + (c2.2 as f32 - c1.2 as f32) * factor).round() as u8;
    Color::Rgb { r, g, b }
}

fn get_theme_color(theme: VisualizerTheme, row: usize, height: usize, is_paused: bool) -> Color {
    if is_paused {
        return Color::Rgb { r: 85, g: 95, b: 110 }; // sleek blue-slate for paused state
    }
    let y = (row - 1) as f32 / (height - 1) as f32;
    match theme {
        VisualizerTheme::Cyberpunk => {
            if y < 0.5 {
                interpolate_color((106, 0, 244), (0, 245, 212), y * 2.0)
            } else {
                interpolate_color((0, 245, 212), (255, 0, 127), (y - 0.5) * 2.0)
            }
        }
        VisualizerTheme::Sunset => {
            if y < 0.5 {
                interpolate_color((255, 210, 0), (247, 127, 0), y * 2.0)
            } else {
                interpolate_color((247, 127, 0), (214, 40, 40), (y - 0.5) * 2.0)
            }
        }
        VisualizerTheme::Aurora => {
            if y < 0.5 {
                interpolate_color((10, 36, 99), (0, 180, 216), y * 2.0)
            } else {
                interpolate_color((0, 180, 216), (144, 224, 169), (y - 0.5) * 2.0)
            }
        }
        VisualizerTheme::Classic => {
            if y < 0.5 {
                interpolate_color((46, 196, 182), (255, 159, 28), y * 2.0)
            } else {
                interpolate_color((255, 159, 28), (231, 29, 54), (y - 0.5) * 2.0)
            }
        }
    }
}

fn get_peak_color(theme: VisualizerTheme, _peak: f32, _height: usize, is_paused: bool) -> Color {
    if is_paused {
        return Color::Rgb { r: 60, g: 65, b: 75 };
    }
    match theme {
        VisualizerTheme::Cyberpunk => Color::Rgb { r: 255, g: 100, b: 200 },
        VisualizerTheme::Sunset => Color::Rgb { r: 255, g: 255, b: 255 },
        VisualizerTheme::Aurora => Color::Rgb { r: 200, g: 255, b: 220 },
        VisualizerTheme::Classic => Color::Rgb { r: 255, g: 80, b: 80 },
    }
}

fn shuffle_indices(indices: &mut [usize]) {
    use std::time::SystemTime;
    let mut seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(123456789);

    let mut next_random = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed
    };

    if indices.len() <= 1 {
        return;
    }
    for i in (1..indices.len()).rev() {
        let j = (next_random() as usize) % (i + 1);
        indices.swap(i, j);
    }
}

fn regenerate_shuffle_queue(
    shuffle_queue: &mut Vec<usize>,
    playlist_indices: &[usize],
    current_playing: Option<usize>,
) {
    let mut remaining: Vec<usize> = playlist_indices
        .iter()
        .copied()
        .filter(|&idx| Some(idx) != current_playing)
        .collect();
    shuffle_indices(&mut remaining);
    *shuffle_queue = remaining;
}

#[derive(Clone, Debug)]
struct TrackMetadata {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
}

fn read_track_metadata(path: &Path) -> Option<TrackMetadata> {
    use lofty::prelude::*;
    use lofty::probe::Probe;
    let tagged_file = Probe::open(path).ok()?.read().ok()?;
    let tag = tagged_file.primary_tag()?;

    Some(TrackMetadata {
        title: tag.title().as_deref().map(String::from),
        artist: tag.artist().as_deref().map(String::from),
        album: tag.album().as_deref().map(String::from),
    })
}

struct TrackVisual {
    duration: Option<Duration>,
    spectra: Vec<Vec<usize>>,
}

enum AnalysisMessage {
    Preview(TrackVisual),
    Complete(TrackVisual),
    Failed(String),
}

struct PendingAnalysis {
    index: usize,
    receiver: mpsc::Receiver<AnalysisMessage>,
}

#[derive(Default)]
struct MeterState {
    levels: Vec<f32>,
    peaks: Vec<f32>,
}

impl MeterState {
    fn clear(&mut self) {
        self.levels.clear();
        self.peaks.clear();
    }
}

struct MeterColumn {
    level: f32,
    peak: f32,
}

#[derive(Default)]
struct RenderState {
    last_height: usize,
}

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            Clear(ClearType::All),
            Hide
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

// --- Player state ---

/// All mutable player state that persists across loop iterations.
struct PlayerState {
    current_index: usize,
    playing_index: Option<usize>,
    current_visual: Option<TrackVisual>,
    pending_analysis: Option<PendingAnalysis>,
    meter_state: MeterState,
    render_state: RenderState,
    search_query: String,
    is_searching: bool,
    repeat_mode: RepeatMode,
    shuffle: bool,
    shuffle_queue: Vec<usize>,
    current_metadata: Option<TrackMetadata>,
    visualizer_theme: VisualizerTheme,
    status: String,
}

impl PlayerState {
    fn new() -> Self {
        Self {
            current_index: 0,
            playing_index: None,
            current_visual: None,
            pending_analysis: None,
            meter_state: MeterState::default(),
            render_state: RenderState::default(),
            search_query: String::new(),
            is_searching: false,
            repeat_mode: RepeatMode::Off,
            shuffle: false,
            shuffle_queue: Vec::new(),
            current_metadata: None,
            visualizer_theme: VisualizerTheme::Cyberpunk,
            status: String::from(
                "Use / to search, j/k to select, Enter to play, Left/Right to seek.",
            ),
        }
    }

    /// Polls the background analysis thread and updates visual/status accordingly.
    fn process_analysis_update(&mut self, audio_files: &[PathBuf]) {
        let update = self
            .pending_analysis
            .as_ref()
            .map(|pending| (pending.index, pending.receiver.try_recv()));

        match update {
            Some((index, Ok(AnalysisMessage::Preview(visual)))) => {
                if self.playing_index == Some(index) {
                    self.current_visual = Some(visual);
                    self.meter_state.clear();
                }
            }
            Some((index, Ok(AnalysisMessage::Complete(visual)))) => {
                self.pending_analysis = None;
                if self.playing_index == Some(index) {
                    self.current_visual = Some(visual);
                    self.meter_state.clear();
                    self.status = format!("Playing {}.", display_name(&audio_files[index]));
                }
            }
            Some((index, Ok(AnalysisMessage::Failed(error)))) => {
                self.pending_analysis = None;
                if self.playing_index == Some(index) {
                    self.status = format!(
                        "Spectrum unavailable for {}: {error}",
                        display_name(&audio_files[index])
                    );
                }
            }
            Some((_, Err(TryRecvError::Disconnected))) => {
                self.pending_analysis = None;
                self.status = String::from("Spectrum analysis stopped.");
            }
            Some((_, Err(TryRecvError::Empty))) | None => {}
        }
    }

    /// Advances to the next track when the current one has finished playing.
    fn advance_if_finished(
        &mut self,
        player: &Player,
        audio_files: &[PathBuf],
        playlist_indices: &[usize],
    ) {
        let Some(finished_index) = self.playing_index.filter(|_| player.empty()) else {
            return;
        };

        self.playing_index = None;
        self.current_visual = None;
        self.pending_analysis = None;
        self.meter_state.clear();
        self.current_metadata = None;

        let next_index = next_track_index(
            finished_index,
            playlist_indices,
            self.repeat_mode,
            self.shuffle,
            &mut self.shuffle_queue,
        );

        if let Some(next_idx) = next_index {
            self.current_index = next_idx;
            self.start_track(player, audio_files, next_idx);
            self.current_metadata = read_track_metadata(&audio_files[next_idx]);
        } else {
            self.status = String::from("Finished.");
        }
    }

    /// Starts playback of the track at `index`, updating visual and analysis state.
    fn start_track(&mut self, player: &Player, audio_files: &[PathBuf], index: usize) {
        let path = &audio_files[index];
        self.meter_state.clear();

        match start_selected_track(player, path, index) {
            Ok(analysis) => {
                self.playing_index = Some(index);
                self.current_visual = None;
                self.pending_analysis = Some(analysis);
                self.status = format!("Playing {}.", display_name(path));
            }
            Err(error) => {
                self.playing_index = None;
                self.current_visual = None;
                self.pending_analysis = None;
                self.status = format!("Could not play {}: {error}", display_name(path));
            }
        }
    }

    /// Handles a key press. Returns `false` when the user requests to quit.
    fn handle_key(
        &mut self,
        code: KeyCode,
        player: &Player,
        audio_files: &[PathBuf],
        playlist_indices: &[usize],
    ) -> bool {
        if self.is_searching {
            match code {
                KeyCode::Esc => {
                    self.is_searching = false;
                    self.search_query.clear();
                    self.status = String::from("Search cleared.");
                }
                KeyCode::Enter => {
                    self.is_searching = false;
                }
                KeyCode::Backspace => {
                    self.search_query.pop();
                }
                KeyCode::Char(character) => {
                    if !character.is_control() {
                        self.search_query.push(character);
                    }
                }
                _ => {}
            }

            let updated_indices = fuzzy_track_indices(audio_files, &self.search_query);
            if let Some(&index) = updated_indices.first() {
                self.current_index = index;
            }
            return true;
        }

        match code {
            KeyCode::Char('+') => {
                let volume = change_volume(player, VOLUME_STEP);
                self.status = format!("Volume {}.", volume_percent(volume));
            }
            KeyCode::Char('-') => {
                let volume = change_volume(player, -VOLUME_STEP);
                self.status = format!("Volume {}.", volume_percent(volume));
            }
            KeyCode::Left => {
                match seek_by(
                    player,
                    self.current_visual.as_ref(),
                    self.playing_index.is_some(),
                    -SEEK_STEP_SECONDS,
                ) {
                    Ok(position) => {
                        self.meter_state.clear();
                        self.status = format!("Seeked to {}.", format_duration(position));
                    }
                    Err(error) => {
                        self.status = format!("Could not seek: {error}");
                    }
                }
            }
            KeyCode::Right => {
                match seek_by(
                    player,
                    self.current_visual.as_ref(),
                    self.playing_index.is_some(),
                    SEEK_STEP_SECONDS,
                ) {
                    Ok(position) => {
                        self.meter_state.clear();
                        self.status = format!("Seeked to {}.", format_duration(position));
                    }
                    Err(error) => {
                        self.status = format!("Could not seek: {error}");
                    }
                }
            }
            KeyCode::Up => {
                self.current_index = move_selection(playlist_indices, self.current_index, -1);
            }
            KeyCode::Down => {
                self.current_index = move_selection(playlist_indices, self.current_index, 1);
            }
            KeyCode::Char('/') => {
                self.is_searching = true;
                self.status = String::from("Type to fuzzy-search tracks. Esc clears search.");
            }
            // 'a' and 'r' are aliases — both toggle the repeat mode.
            KeyCode::Char('a' | 'A' | 'r' | 'R') => {
                self.repeat_mode = self.repeat_mode.next();
                self.status = format!("Repeat mode set to {:?}.", self.repeat_mode);
            }
            KeyCode::Char('s' | 'S') => {
                self.shuffle = !self.shuffle;
                if self.shuffle {
                    regenerate_shuffle_queue(
                        &mut self.shuffle_queue,
                        playlist_indices,
                        self.playing_index,
                    );
                    self.status = String::from("Shuffle enabled.");
                } else {
                    self.shuffle_queue.clear();
                    self.status = String::from("Shuffle disabled.");
                }
            }
            KeyCode::Char('t' | 'T') => {
                self.visualizer_theme = self.visualizer_theme.next();
                self.status = format!("Visualizer theme set to {}.", self.visualizer_theme.name());
            }
            KeyCode::Char(' ') => {
                if let Some(index) = self.playing_index {
                    if player.is_paused() {
                        player.play();
                        self.status = format!("Playing {}.", display_name(&audio_files[index]));
                    } else {
                        player.pause();
                        self.status = format!("Paused {}.", display_name(&audio_files[index]));
                    }
                } else {
                    if !playlist_indices.contains(&self.current_index) {
                        self.status = String::from("No matching track selected.");
                        return true;
                    }

                    let index = self.current_index;
                    self.start_track(player, audio_files, index);
                    self.current_metadata = read_track_metadata(&audio_files[index]);
                    if self.shuffle {
                        regenerate_shuffle_queue(
                            &mut self.shuffle_queue,
                            playlist_indices,
                            self.playing_index,
                        );
                    }
                }
            }
            KeyCode::Enter => {
                if !playlist_indices.contains(&self.current_index) {
                    self.status = String::from("No matching track selected.");
                    return true;
                }

                player.stop();
                let index = self.current_index;
                self.start_track(player, audio_files, index);
                self.current_metadata = read_track_metadata(&audio_files[index]);
                if self.shuffle {
                    regenerate_shuffle_queue(
                        &mut self.shuffle_queue,
                        playlist_indices,
                        self.playing_index,
                    );
                }
            }
            KeyCode::Esc => {
                if self.search_query.is_empty() {
                    return false;
                }
                self.search_query.clear();
                self.current_index = 0;
                self.status = String::from("Search cleared.");
            }
            KeyCode::Char('q') => return false,
            _ => {}
        }

        true
    }
}

// --- Render context ---

/// A snapshot of all the data needed to render a single frame.
struct PlaybackContext<'a> {
    audio_files: &'a [PathBuf],
    playlist_indices: &'a [usize],
    current_index: usize,
    playing_index: Option<usize>,
    status: &'a str,
    elapsed: Duration,
    visual: Option<&'a TrackVisual>,
    metadata: Option<&'a TrackMetadata>,
    is_paused: bool,
    volume: f32,
    is_loading: bool,
    repeat_mode: RepeatMode,
    shuffle: bool,
    visualizer_theme: VisualizerTheme,
    search_query: &'a str,
    is_searching: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let audio_files = audio_files_in(&args.directory)?;

    if audio_files.is_empty() {
        println!("No audio files found in {}.", args.directory.display());
        return Ok(());
    }

    let _terminal = TerminalGuard::new()?;
    let device_sink = rodio::DeviceSinkBuilder::open_default_sink()?;
    let player = Player::connect_new(device_sink.mixer());

    let mut state = PlayerState::new();

    loop {
        let playlist_indices = fuzzy_track_indices(&audio_files, &state.search_query);

        state.process_analysis_update(&audio_files);
        state.advance_if_finished(&player, &audio_files, &playlist_indices);

        if !playlist_indices.contains(&state.current_index) {
            if let Some(&index) = playlist_indices.first() {
                state.current_index = index;
            }
        }

        let ctx = PlaybackContext {
            audio_files: &audio_files,
            playlist_indices: &playlist_indices,
            current_index: state.current_index,
            playing_index: state.playing_index,
            status: &state.status,
            elapsed: player.get_pos(),
            visual: state.current_visual.as_ref(),
            metadata: state.current_metadata.as_ref(),
            is_paused: player.is_paused(),
            volume: player.volume(),
            is_loading: state.pending_analysis.is_some(),
            repeat_mode: state.repeat_mode,
            shuffle: state.shuffle,
            visualizer_theme: state.visualizer_theme,
            search_query: &state.search_query,
            is_searching: state.is_searching,
        };
        draw(&ctx, &mut state.meter_state, &mut state.render_state)?;

        if !event::poll(Duration::from_millis(90))? {
            continue;
        }

        if let Event::Key(key_event) = event::read()? {
            if key_event.kind != KeyEventKind::Press {
                continue;
            }

            if !state.handle_key(key_event.code, &player, &audio_files, &playlist_indices) {
                break;
            }
        }
    }

    player.stop();
    execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
    Ok(())
}

fn audio_files_in(directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut audio_files = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_supported_audio_file(path))
        .collect::<Vec<_>>();

    audio_files.sort();
    Ok(audio_files)
}

fn fuzzy_track_indices(audio_files: &[PathBuf], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..audio_files.len()).collect();
    }

    let mut matches = audio_files
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            fuzzy_score(&display_name(path), query).map(|score| (index, score, display_name(path)))
        })
        .collect::<Vec<_>>();

    matches.sort_by(|(_, left_score, left_name), (_, right_score, right_name)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_name.cmp(right_name))
    });

    matches
        .into_iter()
        .map(|(index, _, _)| index)
        .collect::<Vec<_>>()
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<usize> {
    let candidate = candidate.to_ascii_lowercase();
    let query = query.to_ascii_lowercase();
    let mut score = 0;
    let mut last_match = None;
    let mut search_start = 0;

    for query_char in query.chars().filter(|character| !character.is_whitespace()) {
        let offset = candidate[search_start..].find(query_char)?;
        let position = search_start + offset;

        score += 10;
        if position == 0 {
            score += 8;
        }
        if candidate[..position].ends_with([' ', '-', '_', '.', '/']) {
            score += 6;
        }
        if last_match.is_some_and(|last| position == last + 1) {
            score += 4;
        }

        last_match = Some(position);
        search_start = position + query_char.len_utf8();
    }

    Some(score)
}

fn move_selection(indices: &[usize], current_index: usize, direction: isize) -> usize {
    if indices.is_empty() {
        return current_index;
    }

    let current_position = indices
        .iter()
        .position(|index| *index == current_index)
        .unwrap_or(0);
    let next_position = current_position
        .saturating_add_signed(direction)
        .min(indices.len() - 1);

    indices[next_position]
}

fn next_playlist_index(indices: &[usize], current_index: usize) -> Option<usize> {
    let current_position = indices.iter().position(|index| *index == current_index)?;
    let next_position = current_position + 1;

    indices
        .get(next_position)
        .copied()
        .or_else(|| indices.first().copied())
        .filter(|index| *index != current_index || indices.len() > 1)
}

/// Determines the index of the next track to play after `finished_index` has ended.
///
/// Returns `None` when playback should stop (end of playlist, no repeat).
fn next_track_index(
    finished_index: usize,
    playlist_indices: &[usize],
    repeat_mode: RepeatMode,
    shuffle: bool,
    shuffle_queue: &mut Vec<usize>,
) -> Option<usize> {
    if repeat_mode == RepeatMode::One {
        return Some(finished_index);
    }

    if shuffle {
        // Drop any queued indices that are no longer in the visible playlist.
        shuffle_queue.retain(|idx| playlist_indices.contains(idx));

        if shuffle_queue.is_empty() {
            if repeat_mode == RepeatMode::All {
                regenerate_shuffle_queue(shuffle_queue, playlist_indices, Some(finished_index));
                shuffle_queue.pop()
            } else {
                None
            }
        } else {
            Some(shuffle_queue.remove(0))
        }
    } else {
        let next_idx = next_playlist_index(playlist_indices, finished_index)?;

        // Detect wrap-around: if next position is at or before current, we looped.
        let pos_curr = playlist_indices.iter().position(|&x| x == finished_index);
        let pos_next = playlist_indices.iter().position(|&x| x == next_idx);

        match (pos_curr, pos_next) {
            (Some(curr), Some(nxt)) if nxt <= curr => {
                // Wrapped back to the start of the playlist.
                (repeat_mode == RepeatMode::All).then_some(next_idx)
            }
            _ => Some(next_idx),
        }
    }
}

fn change_volume(player: &Player, delta: f32) -> f32 {
    let volume = (player.volume() + delta).clamp(0.0, MAX_VOLUME);
    player.set_volume(volume);
    volume
}

fn seek_by(
    player: &Player,
    visual: Option<&TrackVisual>,
    is_playing: bool,
    delta_seconds: i64,
) -> Result<Duration, Box<dyn Error>> {
    if !is_playing {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "nothing is playing").into());
    }

    let current = player.get_pos();
    let new_position = if delta_seconds.is_negative() {
        current.saturating_sub(Duration::from_secs(delta_seconds.unsigned_abs()))
    } else {
        current.saturating_add(Duration::from_secs(delta_seconds as u64))
    };
    let new_position = visual
        .and_then(|visual| visual.duration)
        .map(|duration| new_position.min(duration))
        .unwrap_or(new_position);

    player.try_seek(new_position)?;
    Ok(new_position)
}

fn volume_percent(volume: f32) -> String {
    format!("{:.0}%", volume * 100.0)
}

fn is_supported_audio_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("aif" | "aiff" | "aifc" | "mp3" | "wav")
    )
}

fn start_selected_track(
    player: &Player,
    path: &Path,
    index: usize,
) -> Result<PendingAnalysis, Box<dyn Error>> {
    start_playback(player, path)?;
    Ok(analyze_track_in_background(index, path.to_path_buf()))
}

fn start_playback(player: &Player, path: &Path) -> Result<(), Box<dyn Error>> {
    let file = File::open(path)?;
    let source = Decoder::try_from(file)?;
    player.append(source);

    Ok(())
}

fn analyze_track_in_background(index: usize, path: PathBuf) -> PendingAnalysis {
    let (sender, receiver) = mpsc::channel();

    thread::spawn(move || {
        match analyze_track_preview(&path, METER_HEIGHT) {
            Ok(visual) if !visual.spectra.is_empty() => {
                if sender.send(AnalysisMessage::Preview(visual)).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(error) => {
                let _ = sender.send(AnalysisMessage::Failed(error.to_string()));
                return;
            }
        }

        let message = match analyze_track(&path, METER_HEIGHT) {
            Ok(visual) => AnalysisMessage::Complete(visual),
            Err(error) => AnalysisMessage::Failed(error.to_string()),
        };
        let _ = sender.send(message);
    });

    PendingAnalysis { index, receiver }
}

fn analyze_track_preview(path: &Path, height: usize) -> Result<TrackVisual, Box<dyn Error>> {
    analyze_track_with_limit(
        path,
        height,
        Some(Duration::from_secs(SPECTRUM_PREVIEW_SECONDS)),
    )
}

fn analyze_track(path: &Path, height: usize) -> Result<TrackVisual, Box<dyn Error>> {
    analyze_track_with_limit(path, height, None)
}

fn analyze_track_with_limit(
    path: &Path,
    height: usize,
    sample_limit: Option<Duration>,
) -> Result<TrackVisual, Box<dyn Error>> {
    let file = File::open(path)?;
    let mut source = Decoder::try_from(file)?;
    let duration = source.total_duration();
    let channels = usize::from(source.channels().get());
    let sample_rate = source.sample_rate().get() as usize;
    let hop_size = (sample_rate / SPECTRUM_FPS as usize).max(1);
    let max_mono_samples = sample_limit.map(|limit| limit.as_secs_f64() * sample_rate as f64);
    let mut mono_samples = Vec::new();
    let mut frame_sum = 0.0;
    let mut channel_index = 0;

    for sample in source.by_ref() {
        frame_sum += f64::from(sample);
        channel_index += 1;

        if channel_index == channels {
            mono_samples.push((frame_sum / channels as f64) as f32);
            frame_sum = 0.0;
            channel_index = 0;

            if max_mono_samples.is_some_and(|limit| mono_samples.len() as f64 >= limit) {
                break;
            }
        }
    }

    let spectra = analyze_spectra(&mono_samples, sample_rate, hop_size, height);

    Ok(TrackVisual { duration, spectra })
}

fn analyze_spectra(
    samples: &[f32],
    sample_rate: usize,
    hop_size: usize,
    height: usize,
) -> Vec<Vec<usize>> {
    if samples.is_empty() {
        return Vec::new();
    }

    let starts = (0..samples.len()).step_by(hop_size).collect::<Vec<_>>();
    let raw_spectra = analyze_raw_spectra(samples, sample_rate, &starts);
    let max_magnitude = raw_spectra
        .iter()
        .flatten()
        .copied()
        .fold(0.0_f32, f32::max);

    raw_spectra
        .into_iter()
        .map(|bands| {
            bands
                .into_iter()
                .map(|band| magnitude_to_height(band, max_magnitude, height))
                .collect()
        })
        .collect()
}

fn analyze_raw_spectra(samples: &[f32], sample_rate: usize, starts: &[usize]) -> Vec<Vec<f32>> {
    let available_threads = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let worker_count = available_threads
        .saturating_sub(ANALYSIS_RESERVED_THREADS)
        .max(1)
        .min(starts.len())
        .max(1);

    if worker_count == 1 {
        return analyze_raw_spectra_chunk(samples, sample_rate, starts);
    }

    let chunk_size = starts.len().div_ceil(worker_count);

    thread::scope(|scope| {
        starts
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || analyze_raw_spectra_chunk(samples, sample_rate, chunk))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|handle| handle.join().expect("spectrum analysis worker panicked"))
            .collect()
    })
}

fn analyze_raw_spectra_chunk(
    samples: &[f32],
    sample_rate: usize,
    starts: &[usize],
) -> Vec<Vec<f32>> {
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let window = (0..FFT_SIZE)
        .map(|index| hann_window(index, FFT_SIZE))
        .collect::<Vec<_>>();

    starts
        .iter()
        .map(|start| {
            let mut input = vec![Complex::new(0.0_f32, 0.0); FFT_SIZE];

            for (offset, sample) in samples[*start..].iter().take(FFT_SIZE).enumerate() {
                input[offset].re = *sample * window[offset];
            }

            fft.process(&mut input);
            frequency_bands(&input, sample_rate)
        })
        .collect()
}

fn frequency_bands(input: &[Complex<f32>], sample_rate: usize) -> Vec<f32> {
    let min_frequency = 40.0_f32;
    let max_frequency = (sample_rate as f32 / 2.0).min(16_000.0);
    let min_log = min_frequency.log10();
    let max_log = max_frequency.log10();

    (0..SPECTRUM_BANDS)
        .map(|band| {
            let start_frequency = 10.0_f32
                .powf(min_log + (band as f32 / SPECTRUM_BANDS as f32) * (max_log - min_log));
            let end_frequency = 10.0_f32
                .powf(min_log + ((band + 1) as f32 / SPECTRUM_BANDS as f32) * (max_log - min_log));
            let start_bin = frequency_to_bin(start_frequency, sample_rate);
            let end_bin = frequency_to_bin(end_frequency, sample_rate).max(start_bin + 1);
            let end_bin = end_bin.min(input.len() / 2);

            let sum = input[start_bin..end_bin]
                .iter()
                .map(|sample| sample.norm())
                .sum::<f32>();

            sum / (end_bin - start_bin) as f32
        })
        .collect()
}

fn frequency_to_bin(frequency: f32, sample_rate: usize) -> usize {
    ((frequency / sample_rate as f32) * FFT_SIZE as f32).round() as usize
}

fn hann_window(index: usize, size: usize) -> f32 {
    let phase = (2.0 * std::f32::consts::PI * index as f32) / (size - 1) as f32;
    0.5 * (1.0 - phase.cos())
}

fn magnitude_to_height(magnitude: f32, max_magnitude: f32, height: usize) -> usize {
    if max_magnitude <= f32::EPSILON {
        return 1;
    }

    let normalized = (magnitude / max_magnitude).sqrt();
    (normalized * height as f32)
        .round()
        .clamp(1.0, height as f32) as usize
}

// --- Rendering ---

fn draw(
    ctx: &PlaybackContext<'_>,
    meter_state: &mut MeterState,
    render_state: &mut RenderState,
) -> io::Result<()> {
    let mut frame = Vec::new();

    let width = terminal_width();
    write_header(&mut frame, width)?;
    write_playback_panel(&mut frame, ctx, width)?;
    write_spectrum_panel(&mut frame, ctx, width, meter_state)?;
    write_playlist_panel(&mut frame, ctx, width)?;

    let frame_height = frame.iter().filter(|byte| **byte == b'\n').count();
    let should_clear_tail = frame_height < render_state.last_height;
    let mut output = Vec::with_capacity(
        BEGIN_SYNCHRONIZED_UPDATE.len()
            + MOVE_CURSOR_HOME.len()
            + frame.len()
            + CLEAR_FROM_CURSOR_DOWN.len()
            + END_SYNCHRONIZED_UPDATE.len(),
    );
    output.extend_from_slice(BEGIN_SYNCHRONIZED_UPDATE);
    output.extend_from_slice(MOVE_CURSOR_HOME);
    output.extend_from_slice(&frame);
    if should_clear_tail {
        output.extend_from_slice(CLEAR_FROM_CURSOR_DOWN);
    }
    output.extend_from_slice(END_SYNCHRONIZED_UPDATE);

    let mut stdout = io::stdout();
    stdout.write_all(&output)?;
    render_state.last_height = frame_height;
    stdout.flush()
}

fn write_header(stdout: &mut impl Write, width: usize) -> io::Result<()> {
    let title = " eurhythm ";
    let controls = " / search  r repeat  s shuffle  t theme  Enter play  q quit ";
    let fill = width.saturating_sub(2 + title.chars().count() + controls.chars().count());

    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "┌")?;
    execute!(stdout, SetForegroundColor(Color::Green))?;
    write!(stdout, "{title}")?;
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "{}", "─".repeat(fill))?;
    execute!(stdout, SetForegroundColor(Color::White))?;
    write!(stdout, "{controls}")?;
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "┐\r\n")?;
    execute!(stdout, ResetColor)
}

fn write_playback_panel(
    stdout: &mut impl Write,
    ctx: &PlaybackContext<'_>,
    width: usize,
) -> io::Result<()> {
    write_panel_top(stdout, "playback", width, Color::Green)?;

    let now_playing = ctx
        .playing_index
        .map(|index| display_name(&ctx.audio_files[index]))
        .unwrap_or_else(|| String::from("none"));

    let state_str = match (ctx.playing_index.is_some(), ctx.is_paused) {
        (false, _) => "■ IDLE",
        (true, true) => "⏸ PAUSED",
        (true, false) => "▶ PLAYING",
    };
    let state_color = if ctx.playing_index.is_none() {
        Color::DarkGrey
    } else if ctx.is_paused {
        Color::Yellow
    } else {
        Color::Green
    };

    let vol_percent = volume_percent(ctx.volume);

    // Normal part: 0% to 100% (volume 0.0 to 1.0)
    let vol_norm = ctx.volume.min(1.0);
    let filled_norm = (vol_norm * 5.0).round() as usize;
    let empty_norm = 5 - filled_norm;
    let normal_filled = "▮".repeat(filled_norm);
    let normal_empty = "░".repeat(empty_norm);

    // Boost part: 100% to 200% (volume 1.0 to 2.0). Only present when volume > 1.0.
    let boost_filled = if ctx.volume > 1.0 {
        let vol_boost = ctx.volume - 1.0;
        let filled_boost = (vol_boost * 5.0).round() as usize;
        "▮".repeat(filled_boost)
    } else {
        String::new()
    };

    let repeat_str = match ctx.repeat_mode {
        RepeatMode::Off => "→ OFF",
        RepeatMode::All => "🔁 ALL",
        RepeatMode::One => "🔂 ONE",
    };
    let repeat_color = match ctx.repeat_mode {
        RepeatMode::Off => Color::DarkGrey,
        _ => Color::Cyan,
    };

    let shuffle_str = if ctx.shuffle { "🔀 ON" } else { "→ OFF" };
    let shuffle_color = if ctx.shuffle { Color::Cyan } else { Color::DarkGrey };

    let theme_str = ctx.visualizer_theme.name();

    write_panel_line(
        stdout,
        width,
        &[
            ("state", Color::Green),
            (" ", Color::DarkGrey),
            (state_str, state_color),
            ("  volume", Color::Green),
            (" ", Color::DarkGrey),
            (&vol_percent, Color::White),
            (" [", Color::DarkGrey),
            (&normal_filled, Color::Cyan),
            (&normal_empty, Color::DarkGrey),
            (&boost_filled, Color::Red),
            ("]", Color::DarkGrey),
            ("  theme", Color::Green),
            (" ", Color::DarkGrey),
            (theme_str, Color::White),
        ],
    )?;

    write_panel_line(
        stdout,
        width,
        &[
            ("repeat", Color::Green),
            (" ", Color::DarkGrey),
            (repeat_str, repeat_color),
            ("  shuffle", Color::Green),
            (" ", Color::DarkGrey),
            (shuffle_str, shuffle_color),
        ],
    )?;

    if ctx.playing_index.is_some() {
        if let Some(meta) = ctx.metadata {
            let title_str = meta.title.as_deref().unwrap_or(&now_playing);
            write_panel_text(stdout, width, "title", title_str, Color::White)?;
            if let Some(ref artist) = meta.artist {
                write_panel_text(stdout, width, "artist", artist, Color::White)?;
            }
            if let Some(ref album) = meta.album {
                write_panel_text(stdout, width, "album", album, Color::DarkGrey)?;
            }
        } else {
            write_panel_text(stdout, width, "track", &now_playing, Color::White)?;
        }
    } else {
        write_panel_text(stdout, width, "track", &now_playing, Color::White)?;
    }

    write_panel_text(stdout, width, "info", ctx.status, Color::DarkGrey)?;

    let duration = ctx.visual.and_then(|visual| visual.duration);
    let time_str = playback_time(ctx.elapsed, duration);
    let time_str_len = time_str.chars().count();

    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "│ ")?;
    execute!(stdout, SetForegroundColor(Color::Green))?;
    write!(stdout, "time: ")?;

    let label_len = "time: ".len();
    let available = width.saturating_sub(4 + label_len + time_str_len + 1);

    if available > 4 {
        let filled_chars = duration
            .filter(|d| !d.is_zero())
            .map(|d| {
                let progress = ctx.elapsed.as_secs_f64() / d.as_secs_f64();
                (progress.clamp(0.0, 1.0) * available as f64).round() as usize
            })
            .unwrap_or(0);

        let filled_bar_len = filled_chars.saturating_sub(1);
        let unfilled_bar_len = available.saturating_sub(filled_chars);

        execute!(stdout, SetForegroundColor(Color::Rgb { r: 0, g: 245, b: 212 }))?;
        write!(stdout, "{}", "━".repeat(filled_bar_len))?;

        execute!(stdout, SetForegroundColor(Color::White))?;
        if filled_chars > 0 {
            write!(stdout, "●")?;
        } else {
            write!(stdout, "━")?;
        }

        execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
        let empty_char = if ctx.is_loading { "━" } else { "─" };
        write!(stdout, "{}", empty_char.repeat(unfilled_bar_len))?;
    }

    execute!(stdout, SetForegroundColor(Color::White))?;
    write!(stdout, " {time_str}")?;

    let total_written = label_len + available + 1 + time_str_len;
    let padding = width.saturating_sub(4 + total_written);
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "{}│\r\n", " ".repeat(padding))?;
    execute!(stdout, ResetColor)?;

    write_panel_bottom(stdout, width)
}

fn write_spectrum_panel(
    stdout: &mut impl Write,
    ctx: &PlaybackContext<'_>,
    width: usize,
    meter_state: &mut MeterState,
) -> io::Result<()> {
    write_panel_top(stdout, "spectrum", width, Color::Yellow)?;
    write_visualizer(
        stdout,
        ctx.elapsed,
        ctx.visual,
        ctx.is_paused,
        width.saturating_sub(4),
        METER_HEIGHT,
        meter_state,
        ctx.visualizer_theme,
    )?;
    write_panel_bottom(stdout, width)
}

fn write_playlist_panel(
    stdout: &mut impl Write,
    ctx: &PlaybackContext<'_>,
    width: usize,
) -> io::Result<()> {
    let title = playlist_title(ctx.search_query, ctx.is_searching, ctx.playlist_indices.len());
    write_panel_top(stdout, &title, width, Color::Red)?;

    let visible_rows = playlist_visible_rows();
    let current_position = ctx
        .playlist_indices
        .iter()
        .position(|index| *index == ctx.current_index)
        .unwrap_or(0);
    let start_position = current_position.saturating_sub(visible_rows.saturating_sub(1));
    let inner_width = width.saturating_sub(4);

    for &index in ctx
        .playlist_indices
        .iter()
        .skip(start_position)
        .take(visible_rows)
    {
        let path = &ctx.audio_files[index];
        let is_selected = index == ctx.current_index;
        let is_playing = Some(index) == ctx.playing_index;
        let state_str = if is_playing { " [PLAYING]" } else { "" };
        let name_str = display_name(path);

        execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
        write!(stdout, "│ ")?;

        if is_selected {
            write_playlist_row(
                stdout,
                &name_str,
                state_str,
                inner_width,
                "▶ ",
                Color::Cyan,
                Color::White,
                Color::Green,
                Some(Color::Rgb { r: 40, g: 44, b: 52 }),
            )?;
        } else if is_playing {
            write_playlist_row(
                stdout,
                &name_str,
                state_str,
                inner_width,
                "  ",
                Color::Green,
                Color::Green,
                Color::Green,
                None,
            )?;
        } else {
            write_playlist_row(
                stdout,
                &name_str,
                "",
                inner_width,
                "  ",
                Color::DarkGrey,
                Color::DarkGrey,
                Color::DarkGrey,
                None,
            )?;
        }

        execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
        write!(stdout, " │\r\n")?;
        execute!(stdout, ResetColor)?;
    }

    if ctx.playlist_indices.is_empty() {
        write_panel_text(stdout, width, "", "No matches", Color::DarkGrey)?;
    }

    write_panel_bottom(stdout, width)
}

/// Renders the content of a single playlist row (prefix + name + state badge + padding).
///
/// The caller is responsible for writing the leading "│ " border before this call and
/// the trailing " │\r\n" border after it.
fn write_playlist_row(
    stdout: &mut impl Write,
    name: &str,
    state_str: &str,
    inner_width: usize,
    prefix: &str,
    prefix_color: Color,
    name_color: Color,
    state_color: Color,
    background: Option<Color>,
) -> io::Result<()> {
    // 2 display columns for the prefix ("▶ " or "  "), plus the ASCII state badge.
    let reserved = 2 + state_str.len();
    let max_name_len = inner_width.saturating_sub(reserved);
    let truncated_name = truncate(name, max_name_len);

    if let Some(bg) = background {
        execute!(stdout, SetBackgroundColor(bg))?;
    }

    execute!(stdout, SetForegroundColor(prefix_color))?;
    write!(stdout, "{prefix}")?;

    execute!(stdout, SetForegroundColor(name_color))?;
    write!(stdout, "{truncated_name}")?;

    if !state_str.is_empty() {
        execute!(stdout, SetForegroundColor(state_color))?;
        write!(stdout, "{state_str}")?;
    }

    let written = reserved + display_width(&truncated_name);
    let padding = inner_width.saturating_sub(written);
    write!(stdout, "{}", " ".repeat(padding))?;

    if background.is_some() {
        execute!(stdout, ResetColor)?;
    }

    Ok(())
}

fn playlist_title(search_query: &str, is_searching: bool, match_count: usize) -> String {
    if search_query.is_empty() && !is_searching {
        return String::from("playlist");
    }

    let cursor = if is_searching { "_" } else { "" };
    format!("playlist /{search_query}{cursor} ({match_count})")
}

fn playlist_visible_rows() -> usize {
    size()
        .map(|(_, rows)| {
            usize::from(rows)
                .saturating_sub(NON_PLAYLIST_ROWS)
                .clamp(PLAYLIST_MIN_ROWS, PLAYLIST_MAX_ROWS)
        })
        .unwrap_or(PLAYLIST_MAX_ROWS)
}

fn write_panel_top(
    stdout: &mut impl Write,
    title: &str,
    width: usize,
    color: Color,
) -> io::Result<()> {
    let title = truncate(title, width.saturating_sub(4));
    let title_len = title.chars().count();

    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "┌")?;
    execute!(stdout, SetForegroundColor(color))?;
    write!(stdout, " {title} ")?;
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(
        stdout,
        "{}",
        "─".repeat(width.saturating_sub(title_len + 4))
    )?;
    write!(stdout, "┐\r\n")?;
    execute!(stdout, ResetColor)
}

fn write_panel_bottom(stdout: &mut impl Write, width: usize) -> io::Result<()> {
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "└{}┘\r\n", "─".repeat(width.saturating_sub(2)))?;
    execute!(stdout, ResetColor)
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let cp = c as u32;
            if (cp >= 0x1F300 && cp <= 0x1F9FF) || cp >= 0x1F000 {
                2
            } else {
                1
            }
        })
        .sum()
}

fn write_panel_text(
    stdout: &mut impl Write,
    width: usize,
    label: &str,
    value: &str,
    color: Color,
) -> io::Result<()> {
    let mut text = String::new();
    if !label.is_empty() {
        text.push_str(label);
        text.push_str(": ");
    }
    text.push_str(value);
    let text = truncate(&text, width.saturating_sub(4));
    let text_len = display_width(&text);

    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "│ ")?;
    execute!(stdout, SetForegroundColor(color))?;
    write!(stdout, "{text}")?;
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(
        stdout,
        "{}│\r\n",
        " ".repeat(width.saturating_sub(text_len + 3))
    )?;
    execute!(stdout, ResetColor)
}

fn write_panel_line(
    stdout: &mut impl Write,
    width: usize,
    parts: &[(&str, Color)],
) -> io::Result<()> {
    let content_len = parts.iter().map(|(part, _)| display_width(part)).sum::<usize>();

    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "│ ")?;

    for (part, color) in parts {
        execute!(stdout, SetForegroundColor(*color))?;
        write!(stdout, "{part}")?;
    }

    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(
        stdout,
        "{}│\r\n",
        " ".repeat(width.saturating_sub(content_len + 3))
    )?;
    execute!(stdout, ResetColor)
}

fn terminal_width() -> usize {
    size()
        .map(|(columns, _)| usize::from(columns).saturating_sub(1).clamp(60, 120))
        .unwrap_or(80)
}

fn playback_time(elapsed: Duration, duration: Option<Duration>) -> String {
    match duration {
        Some(duration) => format!(
            "{} / {}",
            format_duration(elapsed),
            format_duration(duration)
        ),
        None => format!("{} / --:--", format_duration(elapsed)),
    }
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;

    format!("{minutes:02}:{seconds:02}")
}

fn write_visualizer(
    stdout: &mut impl Write,
    elapsed: Duration,
    visual: Option<&TrackVisual>,
    is_paused: bool,
    inner_width: usize,
    height: usize,
    meter_state: &mut MeterState,
    visualizer_theme: VisualizerTheme,
) -> io::Result<()> {
    let bar_count = meter_bar_count(inner_width);
    let columns = eased_visualizer_levels(visual, elapsed, bar_count, meter_state);
    let meter_width = meter_content_width(bar_count);
    let left_padding = (inner_width.saturating_sub(meter_width)) / 2;
    let right_padding = inner_width.saturating_sub(meter_width + left_padding);

    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(stdout, "│ {} │\r\n", " ".repeat(inner_width))?;

    for row in (1..=height).rev() {
        execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
        write!(stdout, "│ {}", " ".repeat(left_padding))?;
        execute!(
            stdout,
            SetForegroundColor(get_theme_color(visualizer_theme, row, height, is_paused))
        )?;

        for (column_index, column) in columns.iter().enumerate() {
            if should_draw_peak(row, column) {
                execute!(
                    stdout,
                    SetForegroundColor(get_peak_color(visualizer_theme, column.peak, height, is_paused))
                )?;
                write!(stdout, "{}", "▀".repeat(METER_BAR_WIDTH))?;
                execute!(
                    stdout,
                    SetForegroundColor(get_theme_color(visualizer_theme, row, height, is_paused))
                )?;
            } else {
                let segment = bar_segment(row, column.level);
                write!(stdout, "{}", segment.repeat(METER_BAR_WIDTH))?;
            }

            if column_index + 1 < columns.len() {
                write!(stdout, "{}", " ".repeat(METER_BAR_GAP))?;
            }
        }

        execute!(stdout, ResetColor)?;
        execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
        write!(stdout, "{} │\r\n", " ".repeat(right_padding))?;
    }

    Ok(())
}

fn meter_bar_count(inner_width: usize) -> usize {
    let bar_stride = METER_BAR_WIDTH + METER_BAR_GAP;
    ((inner_width + METER_BAR_GAP) / bar_stride).clamp(1, SPECTRUM_BANDS)
}

fn meter_content_width(bar_count: usize) -> usize {
    bar_count * METER_BAR_WIDTH + bar_count.saturating_sub(1) * METER_BAR_GAP
}

fn eased_visualizer_levels(
    visual: Option<&TrackVisual>,
    elapsed: Duration,
    column_count: usize,
    meter_state: &mut MeterState,
) -> Vec<MeterColumn> {
    let target_levels = visualizer_levels(visual, elapsed, column_count);

    if meter_state.levels.len() != column_count || meter_state.peaks.len() != column_count {
        meter_state.clear();
        meter_state
            .levels
            .extend(target_levels.iter().map(|level| *level as f32));
        meter_state
            .peaks
            .extend(target_levels.iter().map(|level| *level as f32));
    }

    meter_state
        .levels
        .iter_mut()
        .zip(meter_state.peaks.iter_mut())
        .zip(target_levels)
        .map(|((current, peak), target)| {
            let target = target as f32;
            let easing = if target > *current {
                BAR_RISE_EASING
            } else {
                BAR_FALL_EASING
            };

            *current += (target - *current) * easing;
            if target >= *peak {
                *peak = target;
            } else {
                *peak = (*peak - PEAK_FALL_SPEED).max(target).max(1.0);
            }

            MeterColumn {
                level: current.max(1.0),
                peak: *peak,
            }
        })
        .collect()
}

fn should_draw_peak(row: usize, column: &MeterColumn) -> bool {
    let peak_row = column.peak.round() as usize;
    peak_row == row && column.peak - column.level > 0.75
}

fn bar_segment(row: usize, level: f32) -> &'static str {
    let fill = (level - (row - 1) as f32).clamp(0.0, 1.0);

    match (fill * 8.0).round() as usize {
        0 => " ",
        1 | 2 => "░",
        3 | 4 => "▒",
        5 | 6 => "▓",
        _ => "█",
    }
}

fn visualizer_levels(
    visual: Option<&TrackVisual>,
    elapsed: Duration,
    column_count: usize,
) -> Vec<usize> {
    let Some(visual) = visual else {
        return vec![1; column_count];
    };

    if visual.spectra.is_empty() {
        return vec![1; column_count];
    }

    let frame_index = (elapsed.as_secs_f64() * f64::from(SPECTRUM_FPS)).round() as usize;
    let spectrum = visual
        .spectra
        .get(frame_index)
        .or_else(|| visual.spectra.last())
        .expect("checked non-empty spectra");

    (0..column_count)
        .map(|column| {
            let band = column * spectrum.len() / column_count;
            spectrum.get(band).copied().unwrap_or(1)
        })
        .collect()
}

fn truncate(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_owned();
    }

    if max_len <= 1 {
        return String::new();
    }

    let mut truncated = value.chars().take(max_len - 1).collect::<String>();
    truncated.push('~');
    truncated
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<invalid file name>")
        .to_owned()
}
