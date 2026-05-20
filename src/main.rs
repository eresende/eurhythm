use clap::Parser;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    style::{Color, ResetColor, SetForegroundColor},
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

    let mut current_index = 0;
    let mut playing_index = None;
    let mut current_visual = None;
    let mut pending_analysis: Option<PendingAnalysis> = None;
    let mut meter_state = MeterState::default();
    let mut render_state = RenderState::default();
    let mut search_query = String::new();
    let mut is_searching = false;
    let mut continuous_play = false;
    let mut status =
        String::from("Use / to search, j/k to select, Enter to play, Left/Right to seek.");

    loop {
        let analysis_update = pending_analysis
            .as_ref()
            .map(|pending| (pending.index, pending.receiver.try_recv()));

        match analysis_update {
            Some((index, Ok(AnalysisMessage::Preview(visual)))) => {
                if playing_index == Some(index) {
                    current_visual = Some(visual);
                    meter_state.clear();
                }
            }
            Some((index, Ok(AnalysisMessage::Complete(visual)))) => {
                pending_analysis = None;

                if playing_index == Some(index) {
                    current_visual = Some(visual);
                    meter_state.clear();
                    status = format!("Playing {}.", display_name(&audio_files[index]));
                }
            }
            Some((index, Ok(AnalysisMessage::Failed(error)))) => {
                pending_analysis = None;

                if playing_index == Some(index) {
                    status = format!(
                        "Spectrum unavailable for {}: {error}",
                        display_name(&audio_files[index])
                    );
                }
            }
            Some((_, Err(TryRecvError::Disconnected))) => {
                pending_analysis = None;
                status = String::from("Spectrum analysis stopped.");
            }
            Some((_, Err(TryRecvError::Empty))) | None => {}
        }

        let playlist_indices = fuzzy_track_indices(&audio_files, &search_query);
        if let Some(index) = playing_index.filter(|_| player.empty()) {
            let next_index = continuous_play
                .then(|| next_playlist_index(&playlist_indices, index))
                .flatten();

            playing_index = None;
            current_visual = None;
            pending_analysis = None;
            meter_state.clear();

            if let Some(next_index) = next_index {
                current_index = next_index;
                start_track(
                    &player,
                    &audio_files,
                    current_index,
                    &mut playing_index,
                    &mut current_visual,
                    &mut pending_analysis,
                    &mut meter_state,
                    &mut status,
                );
            } else {
                status = String::from("Finished.");
            }
        }

        if !playlist_indices.contains(&current_index) {
            if let Some(index) = playlist_indices.first() {
                current_index = *index;
            }
        }

        draw(
            &audio_files,
            &playlist_indices,
            current_index,
            playing_index,
            &status,
            player.get_pos(),
            current_visual.as_ref(),
            player.is_paused(),
            player.volume(),
            pending_analysis.is_some(),
            continuous_play,
            &search_query,
            is_searching,
            &mut meter_state,
            &mut render_state,
        )?;

        if !event::poll(Duration::from_millis(90))? {
            continue;
        }

        if let Event::Key(key_event) = event::read()? {
            if key_event.kind != KeyEventKind::Press {
                continue;
            }

            if is_searching {
                match key_event.code {
                    KeyCode::Esc => {
                        is_searching = false;
                        search_query.clear();
                        status = String::from("Search cleared.");
                    }
                    KeyCode::Enter => {
                        is_searching = false;
                    }
                    KeyCode::Backspace => {
                        search_query.pop();
                    }
                    KeyCode::Char(character) => {
                        if !character.is_control() {
                            search_query.push(character);
                        }
                    }
                    _ => {}
                }

                let playlist_indices = fuzzy_track_indices(&audio_files, &search_query);
                if let Some(index) = playlist_indices.first() {
                    current_index = *index;
                }
                continue;
            }

            match key_event.code {
                KeyCode::Char('+') => {
                    let volume = change_volume(&player, VOLUME_STEP);
                    status = format!("Volume {}.", volume_percent(volume));
                }
                KeyCode::Char('-') => {
                    let volume = change_volume(&player, -VOLUME_STEP);
                    status = format!("Volume {}.", volume_percent(volume));
                }
                KeyCode::Left => {
                    match seek_by(
                        &player,
                        current_visual.as_ref(),
                        playing_index.is_some(),
                        -SEEK_STEP_SECONDS,
                    ) {
                        Ok(position) => {
                            meter_state.clear();
                            status = format!("Seeked to {}.", format_duration(position));
                        }
                        Err(error) => {
                            status = format!("Could not seek: {error}");
                        }
                    }
                }
                KeyCode::Right => {
                    match seek_by(
                        &player,
                        current_visual.as_ref(),
                        playing_index.is_some(),
                        SEEK_STEP_SECONDS,
                    ) {
                        Ok(position) => {
                            meter_state.clear();
                            status = format!("Seeked to {}.", format_duration(position));
                        }
                        Err(error) => {
                            status = format!("Could not seek: {error}");
                        }
                    }
                }
                KeyCode::Up => {
                    current_index = move_selection(&playlist_indices, current_index, -1);
                }
                KeyCode::Down => {
                    current_index = move_selection(&playlist_indices, current_index, 1);
                }
                KeyCode::Char('/') => {
                    is_searching = true;
                    status = String::from("Type to fuzzy-search tracks. Esc clears search.");
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    continuous_play = !continuous_play;
                    status = format!(
                        "Continuous play {}.",
                        if continuous_play {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                }
                KeyCode::Char(' ') => {
                    if let Some(index) = playing_index {
                        if player.is_paused() {
                            player.play();
                            status = format!("Playing {}.", display_name(&audio_files[index]));
                        } else {
                            player.pause();
                            status = format!("Paused {}.", display_name(&audio_files[index]));
                        }
                    } else {
                        if !playlist_indices.contains(&current_index) {
                            status = String::from("No matching track selected.");
                            continue;
                        }

                        start_track(
                            &player,
                            &audio_files,
                            current_index,
                            &mut playing_index,
                            &mut current_visual,
                            &mut pending_analysis,
                            &mut meter_state,
                            &mut status,
                        );
                    }
                }
                KeyCode::Enter => {
                    if !playlist_indices.contains(&current_index) {
                        status = String::from("No matching track selected.");
                        continue;
                    }

                    player.stop();
                    start_track(
                        &player,
                        &audio_files,
                        current_index,
                        &mut playing_index,
                        &mut current_visual,
                        &mut pending_analysis,
                        &mut meter_state,
                        &mut status,
                    );
                }
                KeyCode::Esc => {
                    if search_query.is_empty() {
                        break;
                    }

                    search_query.clear();
                    current_index = 0;
                    status = String::from("Search cleared.");
                }
                KeyCode::Char('q') => break,
                _ => {}
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

fn start_track(
    player: &Player,
    audio_files: &[PathBuf],
    index: usize,
    playing_index: &mut Option<usize>,
    current_visual: &mut Option<TrackVisual>,
    pending_analysis: &mut Option<PendingAnalysis>,
    meter_state: &mut MeterState,
    status: &mut String,
) {
    let path = &audio_files[index];
    meter_state.clear();

    match start_selected_track(player, path, index) {
        Ok(analysis) => {
            *playing_index = Some(index);
            *current_visual = None;
            *pending_analysis = Some(analysis);
            *status = format!("Playing {}.", display_name(path));
        }
        Err(error) => {
            *playing_index = None;
            *current_visual = None;
            *pending_analysis = None;
            meter_state.clear();
            *status = format!("Could not play {}: {error}", display_name(path));
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
    let position = if delta_seconds.is_negative() {
        current.saturating_sub(Duration::from_secs(delta_seconds.unsigned_abs()))
    } else {
        current.saturating_add(Duration::from_secs(delta_seconds as u64))
    };
    let position = visual
        .and_then(|visual| visual.duration)
        .map(|duration| position.min(duration))
        .unwrap_or(position);

    player.try_seek(position)?;
    Ok(position)
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

fn draw(
    audio_files: &[PathBuf],
    playlist_indices: &[usize],
    current_index: usize,
    playing_index: Option<usize>,
    status: &str,
    elapsed: Duration,
    visual: Option<&TrackVisual>,
    is_paused: bool,
    volume: f32,
    is_loading: bool,
    continuous_play: bool,
    search_query: &str,
    is_searching: bool,
    meter_state: &mut MeterState,
    render_state: &mut RenderState,
) -> io::Result<()> {
    let mut frame = Vec::new();

    let width = terminal_width();
    write_header(&mut frame, width)?;
    write_playback_panel(
        &mut frame,
        audio_files,
        playing_index,
        status,
        elapsed,
        visual,
        is_paused,
        volume,
        is_loading,
        continuous_play,
        width,
    )?;
    write_spectrum_panel(&mut frame, elapsed, visual, is_paused, width, meter_state)?;
    write_playlist_panel(
        &mut frame,
        audio_files,
        playlist_indices,
        current_index,
        playing_index,
        search_query,
        is_searching,
        width,
    )?;

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
    let title = " music_player ";
    let controls = " / search  a auto  Enter play  q quit ";
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
    audio_files: &[PathBuf],
    playing_index: Option<usize>,
    status: &str,
    elapsed: Duration,
    visual: Option<&TrackVisual>,
    is_paused: bool,
    volume: f32,
    is_loading: bool,
    continuous_play: bool,
    width: usize,
) -> io::Result<()> {
    write_panel_top(stdout, "playback", width, Color::Green)?;

    let now_playing = playing_index
        .map(|index| display_name(&audio_files[index]))
        .unwrap_or_else(|| String::from("none"));
    let state = playback_state(playing_index, is_paused);
    let duration = visual.and_then(|visual| visual.duration);
    let volume = volume_percent(volume);
    let mode = if continuous_play { "auto" } else { "manual" };

    write_panel_line(
        stdout,
        width,
        &[
            ("state", Color::Green),
            (" ", Color::DarkGrey),
            (state, status_color(is_paused)),
            ("  volume", Color::Green),
            (" ", Color::DarkGrey),
            (&volume, Color::White),
            ("  mode", Color::Green),
            (" ", Color::DarkGrey),
            (mode, Color::White),
        ],
    )?;
    write_panel_text(stdout, width, "track", &now_playing, Color::White)?;
    write_panel_text(stdout, width, "info", status, Color::DarkGrey)?;
    write_panel_text(
        stdout,
        width,
        "time",
        &progress_line(elapsed, duration, width, is_loading),
        Color::White,
    )?;

    write_panel_bottom(stdout, width)
}

fn write_spectrum_panel(
    stdout: &mut impl Write,
    elapsed: Duration,
    visual: Option<&TrackVisual>,
    is_paused: bool,
    width: usize,
    meter_state: &mut MeterState,
) -> io::Result<()> {
    write_panel_top(stdout, "spectrum", width, Color::Yellow)?;
    write_visualizer(
        stdout,
        elapsed,
        visual,
        is_paused,
        width.saturating_sub(4),
        METER_HEIGHT,
        meter_state,
    )?;
    write_panel_bottom(stdout, width)
}

fn write_playlist_panel(
    stdout: &mut impl Write,
    audio_files: &[PathBuf],
    playlist_indices: &[usize],
    current_index: usize,
    playing_index: Option<usize>,
    search_query: &str,
    is_searching: bool,
    width: usize,
) -> io::Result<()> {
    let title = playlist_title(search_query, is_searching, playlist_indices.len());
    write_panel_top(stdout, &title, width, Color::Red)?;

    let visible_rows = playlist_visible_rows();
    let current_position = playlist_indices
        .iter()
        .position(|index| *index == current_index)
        .unwrap_or(0);
    let start_position = current_position.saturating_sub(visible_rows.saturating_sub(1));

    for index in playlist_indices
        .iter()
        .skip(start_position)
        .take(visible_rows)
    {
        let index = *index;
        let path = &audio_files[index];
        let selector = if index == current_index { ">" } else { " " };
        let state = if Some(index) == playing_index {
            "playing"
        } else {
            ""
        };

        let state_width = if state.is_empty() { 0 } else { state.len() + 1 };
        let name_width = width.saturating_sub(7 + state_width);
        let name = truncate(&display_name(path), name_width);
        let item = if state.is_empty() {
            format!("{selector} {name}")
        } else {
            format!("{selector} {name} {state}")
        };
        let color = if index == current_index {
            Color::White
        } else if Some(index) == playing_index {
            Color::Green
        } else {
            Color::DarkGrey
        };

        write_panel_text(stdout, width, "", &item, color)?;
    }

    if playlist_indices.is_empty() {
        write_panel_text(stdout, width, "", "No matches", Color::DarkGrey)?;
    }

    write_panel_bottom(stdout, width)
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
    let text_len = text.chars().count();

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
    let content_len = parts.iter().map(|(part, _)| part.len()).sum::<usize>();

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

fn status_color(is_paused: bool) -> Color {
    if is_paused {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn playback_state(playing_index: Option<usize>, is_paused: bool) -> &'static str {
    if playing_index.is_none() {
        "idle"
    } else if is_paused {
        "paused"
    } else {
        "playing"
    }
}

fn progress_bar(
    elapsed: Duration,
    duration: Option<Duration>,
    width: usize,
    is_loading: bool,
) -> String {
    let filled = duration
        .filter(|duration| !duration.is_zero())
        .map(|duration| {
            let progress = elapsed.as_secs_f64() / duration.as_secs_f64();
            (progress.clamp(0.0, 1.0) * width as f64).round() as usize
        })
        .unwrap_or(0);
    let empty = if is_loading { "━" } else { "-" };

    format!(
        "[{}{}]",
        "#".repeat(filled),
        empty.repeat(width.saturating_sub(filled))
    )
}

fn progress_line(
    elapsed: Duration,
    duration: Option<Duration>,
    panel_width: usize,
    is_loading: bool,
) -> String {
    let time = playback_time(elapsed, duration);
    let available = panel_width.saturating_sub("time: ".len() + 4);
    let bar_width = available.saturating_sub(time.chars().count() + 3);

    if bar_width == 0 {
        return truncate(&time, available);
    }

    format!(
        "{} {time}",
        progress_bar(elapsed, duration, bar_width, is_loading)
    )
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
            SetForegroundColor(visualizer_color(row, height, is_paused))
        )?;

        for (column_index, column) in columns.iter().enumerate() {
            if should_draw_peak(row, column) {
                execute!(
                    stdout,
                    SetForegroundColor(peak_color(column.peak, height, is_paused))
                )?;
                write!(stdout, "{}", "▀".repeat(METER_BAR_WIDTH))?;
                execute!(
                    stdout,
                    SetForegroundColor(visualizer_color(row, height, is_paused))
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

fn peak_color(peak: f32, height: usize, is_paused: bool) -> Color {
    if is_paused {
        return Color::DarkGrey;
    }

    if peak > height as f32 * 0.72 {
        Color::Red
    } else {
        Color::Yellow
    }
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

fn visualizer_color(row: usize, height: usize, is_paused: bool) -> Color {
    if is_paused {
        return Color::DarkGrey;
    }

    if row > height * 2 / 3 {
        Color::Red
    } else if row > height / 3 {
        Color::Yellow
    } else {
        Color::Green
    }
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
