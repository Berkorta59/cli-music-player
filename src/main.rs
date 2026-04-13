use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, stdout};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
    Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use id3::{Tag, TagLike};
use image::{GenericImageView, imageops::FilterType};
use rand::seq::SliceRandom;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row, Table,
        TableState, Wrap,
    },
};
use rodio::{Decoder, OutputStream, Sink, Source};
use rustfft::{FftPlanner, num_complex::Complex};
use walkdir::WalkDir;

#[derive(serde::Serialize, serde::Deserialize)]
struct Config {
    music_folder: String,
    #[serde(default = "default_theme_name")]
    theme: String,
}

fn default_theme_name() -> String {
    "retro-terminal".to_string()
}

#[derive(Clone)]
struct Song {
    path: String,
    title: String,
    artist: String,
    album: String,
    genre: String,
    duration: Option<Duration>,
    duration_label: String,
}

#[derive(Clone)]
struct Playlist {
    name: String,
    paths: Vec<String>,
}

#[derive(Clone)]
struct AlbumArtCache {
    width: u16,
    height: u16,
    track_idx: usize,
    art: Text<'static>,
}

#[derive(Clone)]
struct ActivePlayback {
    cursor: Arc<AtomicUsize>,
    total_samples: usize,
    channels: u16,
    sample_rate: u32,
}

impl ActivePlayback {
    fn total_duration(&self) -> Duration {
        if self.channels == 0 || self.sample_rate == 0 {
            return Duration::ZERO;
        }

        let frames = self.total_samples as f64 / self.channels as f64;
        Duration::from_secs_f64(frames / self.sample_rate as f64)
    }

    fn elapsed(&self) -> Duration {
        if self.channels == 0 || self.sample_rate == 0 {
            return Duration::ZERO;
        }

        let cursor = self.cursor.load(Ordering::Relaxed).min(self.total_samples);
        let frames = cursor as f64 / self.channels as f64;
        Duration::from_secs_f64(frames / self.sample_rate as f64)
    }

    fn seek_to(&self, target: Duration) {
        let max_cursor = self.total_samples.saturating_sub(1);
        let raw = (target.as_secs_f64() * self.sample_rate as f64 * self.channels as f64) as usize;
        let mut clamped = raw.min(max_cursor);
        let channel_span = usize::from(self.channels.max(1));
        clamped -= clamped % channel_span;
        self.cursor.store(clamped, Ordering::Relaxed);
    }
}

struct SeekableBufferSource {
    samples: Arc<[f32]>,
    cursor: Arc<AtomicUsize>,
    channels: u16,
    sample_rate: u32,
}

#[derive(Clone)]
struct DecodedTrack {
    track_idx: usize,
    samples: Arc<[f32]>,
    channels: u16,
    sample_rate: u32,
}

struct PendingDecode {
    track_idx: usize,
    rx: mpsc::Receiver<Option<(Arc<[f32]>, u16, u32)>>,
}

struct VisualizerState {
    fft_size: usize,
    bands: usize,
    hop_interval: usize,
    sample_counter: usize,
    ring: Vec<f32>,
    ring_cursor: usize,
    filled: usize,
    magnitudes: Vec<f32>,
    fft: Arc<dyn rustfft::Fft<f32>>,
}

impl VisualizerState {
    fn new(fft_size: usize, bands: usize) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_size);
        Self {
            fft_size,
            bands,
            hop_interval: (fft_size / 6).max(1),
            sample_counter: 0,
            ring: vec![0.0; fft_size],
            ring_cursor: 0,
            filled: 0,
            magnitudes: vec![0.0; bands],
            fft,
        }
    }

    fn clear(&mut self) {
        self.ring.fill(0.0);
        self.magnitudes.fill(0.0);
        self.ring_cursor = 0;
        self.filled = 0;
        self.sample_counter = 0;
    }

    fn ingest_sample(&mut self, sample: f32) {
        self.ring[self.ring_cursor] = sample;
        self.ring_cursor = (self.ring_cursor + 1) % self.fft_size;
        self.filled = (self.filled + 1).min(self.fft_size);
        self.sample_counter += 1;

        if self.filled >= self.fft_size / 2 && self.sample_counter >= self.hop_interval {
            self.sample_counter = 0;
            self.recompute_fft();
        }
    }

    fn recompute_fft(&mut self) {
        let mut buffer = Vec::with_capacity(self.fft_size);
        let base = if self.filled < self.fft_size {
            0
        } else {
            self.ring_cursor
        };

        for idx in 0..self.fft_size {
            let sample = self.ring[(base + idx) % self.fft_size];
            let window = 0.5
                - 0.5
                    * (2.0 * std::f32::consts::PI * idx as f32 / (self.fft_size as f32 - 1.0))
                        .cos();
            buffer.push(Complex {
                re: sample * window,
                im: 0.0,
            });
        }

        self.fft.process(&mut buffer);

        let nyquist_bins = self.fft_size / 2;
        let usable_bins = nyquist_bins.saturating_sub(2).max(self.bands);
        let bins_per_band = (usable_bins / self.bands).max(1);
        let mut next = vec![0.0; self.bands];

        for (band_idx, slot) in next.iter_mut().enumerate() {
            let start = 1 + band_idx * bins_per_band;
            let end = (start + bins_per_band).min(nyquist_bins);
            let mut peak = 0.0;
            for bin in start..end {
                let mag = buffer[bin].norm();
                if mag > peak {
                    peak = mag;
                }
            }
            let db_like = (peak + 1e-7).log10().max(-6.0);
            *slot = ((db_like + 6.0) / 6.0).clamp(0.0, 1.0);
        }

        for (current, updated) in self.magnitudes.iter_mut().zip(next.into_iter()) {
            *current = if updated > *current {
                *current * 0.25 + updated * 0.75
            } else {
                *current * 0.86 + updated * 0.14
            };
        }
    }

    fn snapshot(&self, bins: usize) -> Vec<f32> {
        if bins == 0 {
            return Vec::new();
        }
        if bins == self.magnitudes.len() {
            return self.magnitudes.clone();
        }

        let mut resized = vec![0.0; bins];
        let src_len = self.magnitudes.len().max(1);
        for (idx, slot) in resized.iter_mut().enumerate() {
            let src = idx * src_len / bins;
            *slot = self.magnitudes[src.min(src_len - 1)];
        }
        resized
    }
}

struct TapSource<S>
where
    S: Source<Item = f32>,
{
    inner: S,
    visualizer: Arc<Mutex<VisualizerState>>,
}

impl<S> Iterator for TapSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|sample| {
            if let Ok(mut visualizer) = self.visualizer.lock() {
                visualizer.ingest_sample(sample);
            }
            sample
        })
    }
}

impl<S> Source for TapSource<S>
where
    S: Source<Item = f32>,
{
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }

    fn channels(&self) -> u16 {
        self.inner.channels()
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

fn tap_source<S>(source: S, visualizer: Arc<Mutex<VisualizerState>>) -> TapSource<S>
where
    S: Source<Item = f32>,
{
    TapSource {
        inner: source,
        visualizer,
    }
}

impl Iterator for SeekableBufferSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let idx = self.cursor.fetch_add(1, Ordering::Relaxed);
        self.samples.get(idx).copied()
    }
}

impl Source for SeekableBufferSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        if self.channels == 0 || self.sample_rate == 0 {
            return Some(Duration::ZERO);
        }

        let frames = self.samples.len() as f64 / self.channels as f64;
        Some(Duration::from_secs_f64(frames / self.sample_rate as f64))
    }
}

#[derive(PartialEq, Clone, Copy)]
enum AppMode {
    Normal,
    FolderBrowser,
    PathInput,
    PlaylistNameInput,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppTab {
    Queue,
    Directories,
    Artists,
    AlbumArtists,
    Albums,
    Genre,
    Playlists,
    Search,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepeatMode {
    Off,
    Track,
    Playlist,
}

impl RepeatMode {
    fn label(self) -> &'static str {
        match self {
            Self::Off => "Repeat Off",
            Self::Track => "Repeat One",
            Self::Playlist => "Repeat Playlist",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Off => Self::Track,
            Self::Track => Self::Playlist,
            Self::Playlist => Self::Off,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeMode {
    Minimal,
    RetroTerminal,
    Monochrome,
}

impl ThemeMode {
    fn label(self) -> &'static str {
        match self {
            Self::Minimal => "Minimal",
            Self::RetroTerminal => "Retro Terminal",
            Self::Monochrome => "Monochrome",
        }
    }

    fn config_value(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::RetroTerminal => "retro-terminal",
            Self::Monochrome => "monochrome",
        }
    }

    fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "minimal" => Self::Minimal,
            "monochrome" => Self::Monochrome,
            _ => Self::RetroTerminal,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Minimal => Self::RetroTerminal,
            Self::RetroTerminal => Self::Monochrome,
            Self::Monochrome => Self::Minimal,
        }
    }

    fn as_index(self) -> usize {
        match self {
            Self::Minimal => 0,
            Self::RetroTerminal => 1,
            Self::Monochrome => 2,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Minimal,
            2 => Self::Monochrome,
            _ => Self::RetroTerminal,
        }
    }
}

#[derive(Clone, Copy)]
struct ThemePalette {
    primary: Color,
    secondary: Color,
    accent: Color,
    deep_bg: Color,
    panel_bg: Color,
    muted_fg: Color,
}

static ACTIVE_THEME: AtomicUsize = AtomicUsize::new(1);

fn active_theme() -> ThemeMode {
    ThemeMode::from_index(ACTIVE_THEME.load(Ordering::Relaxed))
}

fn theme_palette(theme: ThemeMode) -> ThemePalette {
    match theme {
        ThemeMode::Minimal => ThemePalette {
            primary: Color::Rgb(92, 120, 156),
            secondary: Color::Rgb(122, 146, 176),
            accent: Color::Rgb(168, 186, 208),
            deep_bg: Color::Rgb(18, 22, 28),
            panel_bg: Color::Rgb(30, 36, 44),
            muted_fg: Color::Rgb(154, 164, 176),
        },
        ThemeMode::RetroTerminal => ThemePalette {
            primary: Color::Rgb(121, 255, 102),
            secondary: Color::Rgb(86, 211, 255),
            accent: Color::Rgb(255, 203, 94),
            deep_bg: Color::Rgb(8, 16, 9),
            panel_bg: Color::Rgb(18, 32, 20),
            muted_fg: Color::Rgb(145, 185, 135),
        },
        ThemeMode::Monochrome => ThemePalette {
            primary: Color::Rgb(225, 225, 225),
            secondary: Color::Rgb(180, 180, 180),
            accent: Color::Rgb(245, 245, 245),
            deep_bg: Color::Rgb(12, 12, 12),
            panel_bg: Color::Rgb(24, 24, 24),
            muted_fg: Color::Rgb(130, 130, 130),
        },
    }
}

impl AppTab {
    fn all() -> [Self; 8] {
        [
            Self::Queue,
            Self::Directories,
            Self::Artists,
            Self::AlbumArtists,
            Self::Albums,
            Self::Genre,
            Self::Playlists,
            Self::Search,
        ]
    }

    fn title(self) -> &'static str {
        match self {
            Self::Queue => "Queue",
            Self::Directories => "Directories",
            Self::Artists => "Artists",
            Self::AlbumArtists => "Album Artists",
            Self::Albums => "Albums",
            Self::Genre => "Genre",
            Self::Playlists => "Playlists",
            Self::Search => "Search",
        }
    }
}

struct App {
    songs: Vec<Song>,
    directory_counts: Vec<(String, usize)>,
    artist_counts: Vec<(String, usize)>,
    album_counts: Vec<(String, usize)>,
    genre_counts: Vec<(String, usize)>,
    selected: usize,
    table_state: TableState,
    current_track: Option<usize>,
    play_start: Option<Instant>,
    song_duration: Option<Duration>,
    is_playing: bool,
    paused_elapsed: Duration,
    volume: f32,
    active_playback: Option<ActivePlayback>,
    decoded_track: Option<DecodedTrack>,
    pending_decode: Option<PendingDecode>,
    album_art: Option<Vec<u8>>,
    album_art_cache: Option<AlbumArtCache>,
    active_tab: AppTab,
    music_folder: String,
    mode: AppMode,
    browser_path: PathBuf,
    browser_entries: Vec<PathBuf>,
    browser_state: ListState,
    path_input: String,
    shuffle_enabled: bool,
    shuffle_history: Vec<usize>,
    play_queue: Vec<usize>,
    visualizer: Arc<Mutex<VisualizerState>>,
    repeat_mode: RepeatMode,
    theme_mode: ThemeMode,
    status_message: Option<String>,
    status_until: Option<Instant>,
    playlists: Vec<Playlist>,
    playlist_state: ListState,
    playlist_name_input: String,
}

fn neon_blue() -> Color {
    theme_palette(active_theme()).primary
}

fn neon_purple() -> Color {
    theme_palette(active_theme()).secondary
}

fn neon_pink() -> Color {
    theme_palette(active_theme()).accent
}

fn deep_bg() -> Color {
    theme_palette(active_theme()).deep_bg
}

fn panel_bg() -> Color {
    theme_palette(active_theme()).panel_bg
}

fn muted_fg() -> Color {
    theme_palette(active_theme()).muted_fg
}

fn primary_block(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(neon_blue()))
        .style(Style::default().bg(panel_bg()))
}

fn secondary_block(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(neon_purple()))
        .style(Style::default().bg(panel_bg()))
}

fn title_case_fallback(path: &Path) -> String {
    path.file_stem()
        .map(|name| name.to_string_lossy().replace('_', " "))
        .unwrap_or_else(|| "Unknown Track".to_string())
}

fn load_songs(folder: &str) -> Vec<Song> {
    let mut songs = Vec::new();

    for entry in WalkDir::new(folder).into_iter().filter_map(Result::ok) {
        if entry
            .path()
            .extension()
            .map(|ext| ext.eq_ignore_ascii_case("mp3"))
            .unwrap_or(false)
        {
            songs.push(read_song(entry.path()));
        }
    }

    songs.sort_by(|left, right| {
        left.artist
            .cmp(&right.artist)
            .then(left.album.cmp(&right.album))
            .then(left.title.cmp(&right.title))
            .then(left.path.cmp(&right.path))
    });

    songs
}

fn read_song(path: &Path) -> Song {
    let fallback_title = title_case_fallback(path);
    let path_string = path.display().to_string();
    let duration = get_mp3_duration(&path_string);

    let mut song = Song {
        path: path_string,
        title: fallback_title,
        artist: "Unknown Artist".to_string(),
        album: "Unknown Album".to_string(),
        genre: "Unclassified".to_string(),
        duration,
        duration_label: "--:--".to_string(),
    };

    if let Ok(tag) = Tag::read_from_path(path) {
        if let Some(title) = tag.title() {
            song.title = title.to_string();
        }
        if let Some(artist) = tag.artist() {
            song.artist = artist.to_string();
        }
        if let Some(album) = tag.album() {
            song.album = album.to_string();
        }
        if let Some(genre) = tag.genre() {
            song.genre = genre.to_string();
        }
    }

    song.duration_label = song
        .duration
        .map(format_duration)
        .unwrap_or_else(|| "--:--".to_string());

    song
}

fn get_mp3_duration(path: &str) -> Option<Duration> {
    mp3_duration::from_path(path).ok()
}

fn decode_song_buffer(path: &str) -> Option<(Arc<[f32]>, u16, u32)> {
    let file = BufReader::new(File::open(path).ok()?);
    let decoder = Decoder::new(file).ok()?;
    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    let samples: Vec<f32> = decoder.convert_samples::<f32>().collect();

    if samples.is_empty() {
        None
    } else {
        Some((Arc::from(samples), channels, sample_rate))
    }
}

fn start_background_decode(app: &mut App, track_idx: usize, path: String) {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let decoded = decode_song_buffer(&path);
        let _ = tx.send(decoded);
    });
    app.pending_decode = Some(PendingDecode { track_idx, rx });
}

fn pump_decode_ready(app: &mut App) {
    let Some(pending) = app.pending_decode.take() else {
        return;
    };

    match pending.rx.try_recv() {
        Ok(Some((samples, channels, sample_rate))) => {
            app.decoded_track = Some(DecodedTrack {
                track_idx: pending.track_idx,
                samples,
                channels,
                sample_rate,
            });
        }
        Ok(None) => {
            app.decoded_track = None;
        }
        Err(mpsc::TryRecvError::Empty) => {
            app.pending_decode = Some(pending);
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            app.decoded_track = None;
        }
    }
}

fn switch_to_seekable_cached_source(app: &mut App, sink: &Sink, target: Duration) -> bool {
    let Some(current_idx) = app.current_track else {
        return false;
    };
    let Some(decoded) = &app.decoded_track else {
        return false;
    };
    if decoded.track_idx != current_idx || decoded.samples.is_empty() {
        return false;
    }

    let was_paused = sink.is_paused();
    let playback = ActivePlayback {
        cursor: Arc::new(AtomicUsize::new(0)),
        total_samples: decoded.samples.len(),
        channels: decoded.channels,
        sample_rate: decoded.sample_rate,
    };
    playback.seek_to(target);

    let source = SeekableBufferSource {
        samples: decoded.samples.clone(),
        cursor: playback.cursor.clone(),
        channels: decoded.channels,
        sample_rate: decoded.sample_rate,
    };

    sink.stop();
    sink.append(tap_source(source, app.visualizer.clone()));
    sink.set_volume(app.volume);

    if was_paused {
        sink.pause();
    } else {
        sink.play();
    }

    app.song_duration = Some(playback.total_duration());
    app.active_playback = Some(playback);
    app.play_start = None;
    app.paused_elapsed = target;
    app.is_playing = !was_paused;
    true
}

fn seek_with_stream_decoder(app: &mut App, sink: &Sink, target: Duration) {
    let Some(current_idx) = app.current_track else {
        return;
    };
    let current = &app.songs[current_idx];

    let file = match File::open(&current.path) {
        Ok(file) => file,
        Err(_) => return,
    };
    let reader = BufReader::new(file);
    let source = match Decoder::new(reader) {
        Ok(source) => source.skip_duration(target).convert_samples::<f32>(),
        Err(_) => return,
    };

    let was_paused = sink.is_paused();
    sink.stop();
    sink.append(tap_source(source, app.visualizer.clone()));
    sink.set_volume(app.volume);

    if was_paused {
        sink.pause();
        app.play_start = None;
        app.is_playing = false;
    } else {
        sink.play();
        app.play_start = Some(Instant::now());
        app.is_playing = true;
    }

    app.paused_elapsed = target;
}

fn play_selected_song(app: &mut App, sink: &Sink) {
    if app.songs.is_empty() {
        return;
    }

    sink.stop();
    let current_path = app.songs[app.selected].path.clone();
    let current_duration = app.songs[app.selected].duration;
    let file = match File::open(&current_path) {
        Ok(file) => file,
        Err(_) => {
            app.status_message = Some("Failed to open selected track".to_string());
            app.status_until = Some(Instant::now() + Duration::from_secs(3));
            app.is_playing = false;
            app.active_playback = None;
            return;
        }
    };
    let reader = BufReader::new(file);
    let source = match Decoder::new(reader) {
        Ok(source) => source.convert_samples::<f32>(),
        Err(_) => {
            app.status_message = Some("Failed to decode selected track".to_string());
            app.status_until = Some(Instant::now() + Duration::from_secs(3));
            app.is_playing = false;
            app.active_playback = None;
            return;
        }
    };

    if let Ok(mut visualizer) = app.visualizer.lock() {
        visualizer.clear();
    }

    sink.append(tap_source(source, app.visualizer.clone()));
    sink.play();
    sink.set_volume(app.volume);

    start_background_decode(app, app.selected, current_path.clone());

    app.decoded_track = None;
    app.active_playback = None;
    app.album_art = get_album_art(&current_path);
    app.album_art_cache = None;
    app.song_duration = current_duration;
    app.play_start = Some(Instant::now());
    app.is_playing = true;
    app.paused_elapsed = Duration::ZERO;
    app.current_track = Some(app.selected);
}

fn current_elapsed(app: &App) -> Duration {
    if let Some(playback) = &app.active_playback {
        playback.elapsed()
    } else if app.is_playing {
        if let Some(start) = app.play_start {
            app.paused_elapsed + start.elapsed()
        } else {
            app.paused_elapsed
        }
    } else {
        app.paused_elapsed
    }
}

fn current_song(app: &App) -> Option<&Song> {
    app.current_track
        .and_then(|idx| app.songs.get(idx))
        .or_else(|| app.songs.get(app.selected))
}

fn seek_relative(app: &mut App, sink: &Sink, delta_secs: i64) {
    let Some(current_idx) = app.current_track else {
        return;
    };

    let (current_path, current_duration) = {
        let current = &app.songs[current_idx];
        (current.path.clone(), current.duration)
    };

    let total = app
        .song_duration
        .unwrap_or_else(|| current_duration.unwrap_or(Duration::ZERO));

    if total.is_zero() {
        return;
    }

    let now = current_elapsed(app);
    let now_ms = now.as_millis() as i128;
    let delta_ms = (delta_secs as i128) * 1000;
    let total_ms = total.as_millis() as i128;
    let target_ms = (now_ms + delta_ms).clamp(0, total_ms.saturating_sub(250));
    let target = Duration::from_millis(target_ms as u64);

    if let Some(playback) = &app.active_playback {
        playback.seek_to(target);
        app.song_duration = Some(playback.total_duration());
        app.is_playing = !sink.is_paused();
        return;
    }

    if switch_to_seekable_cached_source(app, sink, target) {
        return;
    }

    // If early seeks happen before background decode is ready, force one decode now
    // so subsequent seek operations use the cached source instead of re-decoding stream.
    let should_force_decode = app
        .pending_decode
        .as_ref()
        .map(|pending| pending.track_idx == current_idx)
        .unwrap_or(false)
        && app
            .decoded_track
            .as_ref()
            .map(|decoded| decoded.track_idx != current_idx)
            .unwrap_or(true);

    if should_force_decode {
        if let Some((samples, channels, sample_rate)) = decode_song_buffer(&current_path) {
            app.pending_decode = None;
            app.decoded_track = Some(DecodedTrack {
                track_idx: current_idx,
                samples,
                channels,
                sample_rate,
            });

            if switch_to_seekable_cached_source(app, sink, target) {
                return;
            }
        }
    }

    seek_with_stream_decoder(app, sink, target);
}

fn play_track_at(app: &mut App, sink: &Sink, track_idx: usize) {
    if track_idx >= app.songs.len() {
        return;
    }

    app.selected = track_idx;
    app.table_state.select(Some(track_idx));
    play_selected_song(app, sink);
}

fn pop_next_queued(app: &mut App) -> Option<usize> {
    while !app.play_queue.is_empty() {
        let idx = app.play_queue.remove(0);
        if idx < app.songs.len() {
            return Some(idx);
        }
    }

    None
}

fn enqueue_selected_track(app: &mut App, play_next: bool) {
    if app.songs.is_empty() {
        app.status_message = Some("Queue is unavailable because no tracks are loaded".to_string());
        app.status_until = Some(Instant::now() + Duration::from_secs(3));
        return;
    }

    let idx = app.selected;
    let title = app
        .songs
        .get(idx)
        .map(|song| song.title.clone())
        .unwrap_or_else(|| "Unknown Track".to_string());

    if play_next {
        app.play_queue.insert(0, idx);
        app.status_message = Some(format!("Play next: {}", title));
    } else {
        app.play_queue.push(idx);
        app.status_message = Some(format!("Queued: {}", title));
    }
    app.status_until = Some(Instant::now() + Duration::from_secs(3));
}

fn pick_shuffle_next(app: &App) -> Option<usize> {
    if app.songs.is_empty() {
        return None;
    }

    if app.songs.len() == 1 {
        return Some(0);
    }

    let current = app.current_track.unwrap_or(app.selected);
    let mut candidates: Vec<usize> = (0..app.songs.len()).filter(|idx| *idx != current).collect();
    candidates.shuffle(&mut rand::thread_rng());
    candidates.first().copied()
}

fn advance_to_next_track(app: &mut App, sink: &Sink) {
    if app.songs.is_empty() {
        return;
    }

    if let Some(queued_idx) = pop_next_queued(app) {
        play_track_at(app, sink, queued_idx);
        return;
    }

    if app.repeat_mode == RepeatMode::Track {
        let replay_idx = app.current_track.unwrap_or(app.selected);
        play_track_at(app, sink, replay_idx);
        return;
    }

    if app.shuffle_enabled {
        let from = app.current_track.unwrap_or(app.selected);
        if let Some(next_idx) = pick_shuffle_next(app) {
            if next_idx != from {
                app.shuffle_history.push(from);
            }
            play_track_at(app, sink, next_idx);
        }
    } else {
        let base = app.current_track.unwrap_or(app.selected);
        if base + 1 < app.songs.len() {
            play_track_at(app, sink, base + 1);
        } else if app.repeat_mode == RepeatMode::Playlist {
            play_track_at(app, sink, 0);
        } else {
            sink.stop();
            app.is_playing = false;
            app.play_start = None;
            app.paused_elapsed = app.song_duration.unwrap_or(Duration::ZERO);
            app.active_playback = None;
            app.status_message = Some("Playback finished (Repeat Off)".to_string());
            app.status_until = Some(Instant::now() + Duration::from_secs(3));
        }
    }
}

fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}", mins, secs)
}

fn get_album_art(path: &str) -> Option<Vec<u8>> {
    let tag = Tag::read_from_path(path).ok()?;
    tag.pictures().next().map(|picture| picture.data.clone())
}

fn playlists_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cli-music-player")
        .join("playlists")
}

fn parse_m3u(path: &Path) -> Option<Playlist> {
    let name = path.file_stem()?.to_string_lossy().to_string();
    let content = std::fs::read_to_string(path).ok()?;
    let paths: Vec<String> = content
        .lines()
        .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .collect();
    Some(Playlist { name, paths })
}

fn load_playlists() -> Vec<Playlist> {
    let dir = playlists_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut playlists = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .map(|ext| ext.eq_ignore_ascii_case("m3u"))
                    .unwrap_or(false)
            })
            .collect();
        paths.sort();
        for path in paths {
            if let Some(pl) = parse_m3u(&path) {
                playlists.push(pl);
            }
        }
    }
    playlists
}

fn playlist_safe_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn save_playlist_m3u(name: &str, paths: &[String]) -> std::io::Result<()> {
    let dir = playlists_dir();
    std::fs::create_dir_all(&dir)?;
    let safe = playlist_safe_name(name);
    let file_path = dir.join(format!("{}.m3u", safe));
    let mut content = "#EXTM3U\n".to_string();
    for path in paths {
        content.push_str(path);
        content.push('\n');
    }
    std::fs::write(&file_path, content)
}

fn delete_playlist_file(name: &str) -> bool {
    let safe = playlist_safe_name(name);
    let file_path = playlists_dir().join(format!("{}.m3u", safe));
    std::fs::remove_file(file_path).is_ok()
}

fn save_current_queue_as_playlist(app: &mut App, name: &str) {
    let mut paths: Vec<String> = Vec::new();
    if let Some(idx) = app.current_track {
        if let Some(song) = app.songs.get(idx) {
            paths.push(song.path.clone());
        }
    }
    for &idx in &app.play_queue {
        if let Some(song) = app.songs.get(idx) {
            paths.push(song.path.clone());
        }
    }
    if paths.is_empty() {
        app.status_message = Some("Queue is empty — nothing to save".to_string());
        app.status_until = Some(Instant::now() + Duration::from_secs(3));
        return;
    }
    match save_playlist_m3u(name, &paths) {
        Ok(()) => {
            app.playlists = load_playlists();
            let saved_len = paths.len();
            app.status_message = Some(format!("Saved '{}' ({} tracks)", name, saved_len));
        }
        Err(e) => {
            app.status_message = Some(format!("Save failed: {}", e));
        }
    }
    app.status_until = Some(Instant::now() + Duration::from_secs(3));
}

fn load_playlist_into_queue(app: &mut App, sink: &Sink, playlist_idx: usize, shuffle: bool) {
    let Some(playlist) = app.playlists.get(playlist_idx) else {
        return;
    };
    let mut indices: Vec<usize> = playlist
        .paths
        .iter()
        .filter_map(|path| app.songs.iter().position(|song| song.path == *path))
        .collect();
    if shuffle {
        indices.shuffle(&mut rand::thread_rng());
    }
    if indices.is_empty() {
        app.status_message = Some("No matching tracks found in library".to_string());
        app.status_until = Some(Instant::now() + Duration::from_secs(3));
        return;
    }
    let count = indices.len();
    let first = indices.remove(0);
    app.play_queue.clear();
    app.play_queue.extend(indices);
    play_track_at(app, sink, first);
    app.status_message = Some(format!("Loaded {} tracks from playlist", count));
    app.status_until = Some(Instant::now() + Duration::from_secs(3));
}

fn render_album_art(data: &[u8], width: u16, height: u16) -> Text<'static> {
    let Ok(img) = image::load_from_memory(data) else {
        return Text::raw("No image");
    };

    let safe_width = width.max(1);
    let safe_height = height.max(1);
    let img = img.resize_exact(
        safe_width as u32,
        safe_height as u32 * 2,
        FilterType::Triangle,
    );

    let mut lines = Vec::new();
    for y in (0..img.height()).step_by(2) {
        let mut spans = Vec::new();
        for x in 0..img.width() {
            let top = img.get_pixel(x, y);
            let bottom = if y + 1 < img.height() {
                img.get_pixel(x, y + 1)
            } else {
                top
            };

            spans.push(Span::styled(
                "▄",
                Style::default()
                    .fg(Color::Rgb(bottom[0], bottom[1], bottom[2]))
                    .bg(Color::Rgb(top[0], top[1], top[2])),
            ));
        }
        lines.push(Line::from(spans));
    }

    Text::from(lines)
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cli-music-player")
        .join("config.json")
}

fn save_config(cfg: &Config) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(&path, json);
    }
}

fn read_browser_entries(path: &PathBuf) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if let Some(parent) = path.parent() {
        if parent != path.as_path() {
            entries.push(parent.to_path_buf());
        }
    }
    if let Ok(read_dir) = std::fs::read_dir(path) {
        let mut dirs: Vec<PathBuf> = read_dir
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|entry| entry.is_dir())
            .collect();
        dirs.sort();
        entries.extend(dirs);
    }
    entries
}

fn centered_rect(percent_x: u16, percent_y: u16, rect: Rect) -> Rect {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(rect);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(layout[1])[1]
}

fn load_config() -> Config {
    let path = config_path();
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(cfg) = serde_json::from_str::<Config>(&content) {
            return cfg;
        }
    }

    Config {
        music_folder: "./music".to_string(),
        theme: default_theme_name(),
    }
}

fn tab_index(tab: AppTab) -> usize {
    AppTab::all()
        .iter()
        .position(|candidate| *candidate == tab)
        .unwrap_or(0)
}

fn next_tab(tab: AppTab) -> AppTab {
    let tabs = AppTab::all();
    tabs[(tab_index(tab) + 1) % tabs.len()]
}

fn previous_tab(tab: AppTab) -> AppTab {
    let tabs = AppTab::all();
    let idx = tab_index(tab);
    tabs[(idx + tabs.len() - 1) % tabs.len()]
}

fn summarize_counts<F>(songs: &[Song], picker: F) -> Vec<(String, usize)>
where
    F: Fn(&Song) -> String,
{
    let mut counts = BTreeMap::new();
    for song in songs {
        let key = picker(song);
        *counts.entry(key).or_insert(0usize) += 1;
    }
    counts.into_iter().collect()
}

fn refresh_library_metrics(app: &mut App) {
    app.directory_counts = summarize_counts(&app.songs, |song| {
        Path::new(&song.path)
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "Library Root".to_string())
    });
    app.artist_counts = summarize_counts(&app.songs, |song| song.artist.clone());
    app.album_counts = summarize_counts(&app.songs, |song| song.album.clone());
    app.genre_counts = summarize_counts(&app.songs, |song| song.genre.clone());
}

fn panel_lines(app: &App) -> Vec<Line<'static>> {
    match app.active_tab {
        AppTab::Queue => {
            let current = current_song(app);
            vec![
                Line::from(vec![
                    Span::styled(
                        "Queue Pulse",
                        Style::default()
                            .fg(neon_pink())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        format!("{} tracks | {} queued", app.songs.len(), app.play_queue.len()),
                        Style::default().fg(muted_fg()),
                    ),
                ]),
                Line::from(vec![Span::styled(
                    current
                        .map(|song| format!("Focused: {}", song.title))
                        .unwrap_or_else(|| "Focused: Nothing loaded".to_string()),
                    Style::default().fg(Color::White),
                )]),
                Line::from(vec![Span::styled(
                    "Enter play, A queue, X play next, Space toggle, Left/Right seek, R shuffle.",
                    Style::default().fg(muted_fg()),
                )]),
            ]
        }
        AppTab::Directories => app
            .directory_counts
            .iter()
            .cloned()
            .into_iter()
        .take(8)
        .map(|(name, count)| {
            Line::from(vec![
                Span::styled(name, Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(
                    format!("{} tracks", count),
                    Style::default().fg(neon_blue()),
                ),
            ])
        })
        .collect(),
        AppTab::Artists | AppTab::AlbumArtists => {
            app.artist_counts
                .iter()
                .cloned()
                .into_iter()
                .rev()
                .take(8)
                .map(|(name, count)| {
                    Line::from(vec![
                        Span::styled(name, Style::default().fg(Color::White)),
                        Span::raw("  "),
                        Span::styled(
                            format!("{} tracks", count),
                            Style::default().fg(neon_purple()),
                        ),
                    ])
                })
                .collect()
        }
        AppTab::Albums => app
            .album_counts
            .iter()
            .cloned()
            .into_iter()
            .rev()
            .take(8)
            .map(|(name, count)| {
                Line::from(vec![
                    Span::styled(name, Style::default().fg(Color::White)),
                    Span::raw("  "),
                    Span::styled(
                        format!("{} tracks", count),
                        Style::default().fg(neon_pink()),
                    ),
                ])
            })
            .collect(),
        AppTab::Genre => app
            .genre_counts
            .iter()
            .cloned()
            .into_iter()
            .rev()
            .take(8)
            .map(|(name, count)| {
                Line::from(vec![
                    Span::styled(name, Style::default().fg(Color::White)),
                    Span::raw("  "),
                    Span::styled(
                        format!("{} tracks", count),
                        Style::default().fg(neon_blue()),
                    ),
                ])
            })
            .collect(),
        AppTab::Playlists => vec![
            Line::from(vec![
                Span::styled(
                    "Playlists",
                    Style::default()
                        .fg(neon_pink())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{} saved  |  .m3u format", app.playlists.len()),
                    Style::default().fg(muted_fg()),
                ),
            ]),
            Line::from(vec![Span::styled(
                "Enter: load  \u{2022}  S: shuffle-load  \u{2022}  s: save queue  \u{2022}  d: delete",
                Style::default().fg(muted_fg()),
            )]),
            Line::from(""),
            Line::from(""),
        ],
        AppTab::Search => vec![
            Line::from(vec![Span::styled(
                "Search Surface",
                Style::default()
                    .fg(neon_blue())
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(format!("Library indexed: {} tracks", app.songs.len())),
            Line::from(format!(
                "Artists: {}  Albums: {}  Genres: {}",
                app.artist_counts.len(),
                app.album_counts.len(),
                app.genre_counts.len(),
            )),
            Line::from("Use the tabs as discovery surfaces while keeping Queue playback active."),
        ],
    }
}

fn visualizer_color(level: f64) -> Color {
    if level < 0.33 {
        neon_pink()
    } else if level < 0.66 {
        neon_purple()
    } else {
        neon_blue()
    }
}

fn build_visualizer(width: u16, height: u16, bins: &[f32]) -> Text<'static> {
    let columns = usize::from((width / 2).max(1));
    let rows = usize::from(height.max(1));

    let mut amplitudes = vec![0usize; columns];
    for (idx, amp) in amplitudes.iter_mut().enumerate() {
        let value = bins.get(idx).copied().unwrap_or(0.0).clamp(0.0, 1.0);
        *amp = ((value * rows as f32).round() as usize).max(1);
    }

    let mut lines = Vec::with_capacity(rows);
    for row in (0..rows).rev() {
        let mut spans = Vec::with_capacity(columns * 2);
        for amplitude in &amplitudes {
            if *amplitude > row {
                let level = (*amplitude as f64 / rows as f64).clamp(0.0, 1.0);
                spans.push(Span::styled(
                    "█",
                    Style::default().fg(visualizer_color(level)),
                ));
                spans.push(Span::styled(" ", Style::default().fg(panel_bg())));
            } else {
                spans.push(Span::styled(" ", Style::default().bg(panel_bg())));
                spans.push(Span::styled(" ", Style::default().bg(panel_bg())));
            }
        }
        lines.push(Line::from(spans));
    }

    Text::from(lines)
}

fn volume_meter(volume: f32) -> String {
    let steps = (volume.clamp(0.0, 2.0) * 4.0).round() as usize;
    let glyphs = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if steps == 0 {
        return "▁".to_string();
    }
    glyphs.iter().take(steps.min(glyphs.len())).collect()
}

fn render_now_playing_bar(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let block = primary_block(Line::from(vec![Span::styled(
        " Now Playing ",
        Style::default()
            .fg(neon_pink())
            .add_modifier(Modifier::BOLD),
    )]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let sections = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(30),
            Constraint::Min(24),
            Constraint::Length(18),
        ])
        .split(inner);

    let state_text = if app.is_playing {
        "[ Playing ]"
    } else {
        "[ Standby ]"
    };
    let shuffle_text = if app.shuffle_enabled {
        "Shuffle On"
    } else {
        "Shuffle Off"
    };
    let playback_modes = format!("{} | {}", shuffle_text, app.repeat_mode.label());
    let queue_text = format!("Queue: {}", app.play_queue.len());
    let theme_text = format!("Theme: {}", app.theme_mode.label());
    let left_text = Paragraph::new(vec![Line::from(vec![Span::styled(
        state_text,
        Style::default()
            .fg(neon_pink())
            .add_modifier(Modifier::BOLD),
    )]), Line::from(vec![Span::styled(
        playback_modes,
        Style::default().fg(neon_blue()),
    )]), Line::from(vec![Span::styled(
        queue_text,
        Style::default().fg(neon_blue()),
    )]), Line::from(vec![Span::styled(
        theme_text,
        Style::default().fg(neon_purple()),
    )])])
    .style(Style::default().bg(panel_bg()));
    f.render_widget(left_text, sections[0]);

    let song = current_song(app);
    let title = song
        .map(|item| item.title.clone())
        .unwrap_or_else(|| "Select a track to begin".to_string());
    let meta = song
        .map(|item| format!("{}  •  {}", item.artist, item.album))
        .unwrap_or_else(|| "Queue, browse, and press Enter to play".to_string());

    let title_widget = Paragraph::new(vec![
        Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(meta, Style::default().fg(muted_fg()))]),
    ])
    .alignment(Alignment::Center)
    .style(Style::default().bg(panel_bg()));
    f.render_widget(title_widget, sections[1]);

    let volume_box = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Volume", Style::default().fg(muted_fg())),
            Span::raw("  "),
            Span::styled(
                format!("{:.0}%", app.volume * 100.0),
                Style::default()
                    .fg(neon_blue())
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![Span::styled(
            volume_meter(app.volume),
            Style::default().fg(neon_purple()),
        )]),
    ])
    .alignment(Alignment::Right)
    .style(Style::default().bg(panel_bg()));
    f.render_widget(volume_box, sections[2]);
}

fn render_art_panel(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let song = current_song(app);
    let title = song
        .map(|track| track.title.clone())
        .unwrap_or_else(|| "Artwork Matrix".to_string());
    let block = primary_block(Line::from(vec![
        Span::styled(
            " Album Art ",
            Style::default()
                .fg(neon_pink())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(title, Style::default().fg(muted_fg())),
    ]));

    if let Some(art_data) = &app.album_art {
        let inner = block.inner(area);
        let track_idx = app.current_track.unwrap_or(app.selected);
        let art = if let Some(cache) = &app.album_art_cache {
            if cache.width == inner.width && cache.height == inner.height && cache.track_idx == track_idx {
                cache.art.clone()
            } else {
                let rendered = render_album_art(art_data, inner.width, inner.height);
                app.album_art_cache = Some(AlbumArtCache {
                    width: inner.width,
                    height: inner.height,
                    track_idx,
                    art: rendered.clone(),
                });
                rendered
            }
        } else {
            let rendered = render_album_art(art_data, inner.width, inner.height);
            app.album_art_cache = Some(AlbumArtCache {
                width: inner.width,
                height: inner.height,
                track_idx,
                art: rendered.clone(),
            });
            rendered
        };
        let widget = Paragraph::new(art)
            .block(block)
            .alignment(Alignment::Center);
        f.render_widget(widget, area);
    } else {
        let placeholder = Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                "████  CYBER  ████",
                Style::default()
                    .fg(neon_purple())
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "     ◢◣",
                Style::default().fg(neon_blue()),
            )]),
            Line::from(vec![Span::styled(
                "   ◢████◣",
                Style::default().fg(neon_pink()),
            )]),
            Line::from(vec![Span::styled(
                "   █ ▓▓ █",
                Style::default().fg(Color::White),
            )]),
            Line::from(vec![Span::styled(
                "   ◥████◤",
                Style::default().fg(neon_blue()),
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "No embedded cover art",
                Style::default().fg(muted_fg()),
            )]),
        ])
        .block(block)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });
        f.render_widget(placeholder, area);
    }
}

fn render_track_panel(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(area);

    let info = Paragraph::new(panel_lines(app))
        .block(secondary_block(Line::from(vec![Span::styled(
            format!(" {} ", app.active_tab.title()),
            Style::default()
                .fg(neon_purple())
                .add_modifier(Modifier::BOLD),
        )])))
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: true });
    f.render_widget(info, chunks[0]);

    if app.active_tab == AppTab::Playlists {
        let items: Vec<ListItem> = if app.playlists.is_empty() {
            vec![ListItem::new(Span::styled(
                "No playlists saved yet. Press [s] to save the current queue.",
                Style::default().fg(muted_fg()),
            ))]
        } else {
            app.playlists
                .iter()
                .map(|pl| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            pl.name.clone(),
                            Style::default()
                                .fg(neon_purple())
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  {} tracks", pl.paths.len()),
                            Style::default().fg(muted_fg()),
                        ),
                    ]))
                })
                .collect()
        };
        let playlist_list = List::new(items)
            .block(primary_block(Line::from(vec![Span::styled(
                " Saved Playlists ",
                Style::default()
                    .fg(neon_blue())
                    .add_modifier(Modifier::BOLD),
            )])))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(155, 122, 255))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▍ ");
        f.render_stateful_widget(playlist_list, chunks[1], &mut app.playlist_state);
        return;
    }

    if app.active_tab != AppTab::Queue {
        let preview_rows: Vec<Row> = app
            .songs
            .iter()
            .take(10)
            .map(|song| {
                Row::new(vec![
                    Cell::from(song.artist.as_str()),
                    Cell::from(song.title.as_str()),
                    Cell::from(song.album.as_str()),
                    Cell::from(song.duration_label.as_str()),
                ])
            })
            .collect();

        let preview = Table::new(
            preview_rows,
            [
                Constraint::Length(18),
                Constraint::Percentage(34),
                Constraint::Percentage(34),
                Constraint::Length(8),
            ],
        )
        .header(
            Row::new(vec!["Artist", "Title", "Album", "Len"]).style(
                Style::default()
                    .fg(neon_blue())
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(primary_block(Line::from(vec![Span::styled(
            " Preview Queue ",
            Style::default()
                .fg(neon_blue())
                .add_modifier(Modifier::BOLD),
        )])));
        f.render_widget(preview, chunks[1]);
        return;
    }

    let rows: Vec<Row> = app
        .songs
        .iter()
        .enumerate()
        .map(|(idx, song)| {
            let mut style = Style::default().fg(muted_fg()).bg(panel_bg());
            if Some(idx) == app.current_track {
                style = style.fg(neon_blue()).add_modifier(Modifier::BOLD);
            }

            Row::new(vec![
                Cell::from(song.artist.as_str()),
                Cell::from(song.title.as_str()),
                Cell::from(song.album.as_str()),
                Cell::from(song.duration_label.as_str()),
            ])
            .style(style)
        })
        .collect();

    let header = Row::new(vec!["Artist", "Title", "Album", "Duration"]).style(
        Style::default()
            .fg(neon_pink())
            .add_modifier(Modifier::BOLD),
    );

    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Percentage(34),
            Constraint::Percentage(34),
            Constraint::Length(9),
        ],
    )
    .header(header)
    .block(primary_block(Line::from(vec![
        Span::styled(
            " Queue Matrix ",
            Style::default()
                .fg(neon_blue())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} tracks loaded", app.songs.len()),
            Style::default().fg(muted_fg()),
        ),
    ])))
    .highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(132, 174, 255))
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▍ ")
    .column_spacing(1);

    f.render_stateful_widget(table, chunks[1], &mut app.table_state);
}

fn render_visualizer(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let columns = usize::from((area.width / 2).max(1));
    let bins = if let Ok(visualizer) = app.visualizer.lock() {
        visualizer.snapshot(columns)
    } else {
        vec![0.0; columns]
    };

    let block = secondary_block(Line::from(vec![
        Span::styled(
            " Reactive Spectrum ",
            Style::default()
                .fg(neon_pink())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("live FFT monitor", Style::default().fg(muted_fg())),
    ]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let visualizer = Paragraph::new(build_visualizer(inner.width, inner.height, &bins))
        .style(Style::default().bg(panel_bg()));
    f.render_widget(visualizer, inner);
}

fn render_footer(
    f: &mut ratatui::Frame,
    area: Rect,
    app: &App,
    elapsed: Duration,
    total: Duration,
    ratio: f64,
) {
    let block = primary_block(Line::from(vec![
        Span::styled(
            " Transport ",
            Style::default()
                .fg(neon_blue())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("playback deck", Style::default().fg(muted_fg())),
    ]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    let timeline = Gauge::default()
        .gauge_style(Style::default().fg(neon_purple()).bg(deep_bg()))
        .ratio(ratio)
        .label(Span::styled(
            format!("{} / {}", format_duration(elapsed), format_duration(total)),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(timeline, rows[0]);

    let controls = Paragraph::new(Line::from(vec![
        Span::styled(
            if app.is_playing {
                "▌▌ Pause"
            } else {
                "▶ Play"
            },
            Style::default()
                .fg(neon_pink())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("Enter", Style::default().fg(neon_blue())),
        Span::styled(" play   ", Style::default().fg(muted_fg())),
        Span::styled("Space", Style::default().fg(neon_blue())),
        Span::styled(" toggle   ", Style::default().fg(muted_fg())),
        Span::styled("←/→", Style::default().fg(neon_blue())),
        Span::styled(" seek   ", Style::default().fg(muted_fg())),
        Span::styled("+/−", Style::default().fg(neon_blue())),
        Span::styled(" volume   ", Style::default().fg(muted_fg())),
        Span::styled("Tab", Style::default().fg(neon_blue())),
        Span::styled(" sections   ", Style::default().fg(muted_fg())),
        Span::styled("r", Style::default().fg(neon_blue())),
        Span::styled(" shuffle   ", Style::default().fg(muted_fg())),
        Span::styled("a", Style::default().fg(neon_blue())),
        Span::styled(" queue   ", Style::default().fg(muted_fg())),
        Span::styled("x", Style::default().fg(neon_blue())),
        Span::styled(" play next   ", Style::default().fg(muted_fg())),
        Span::styled("m", Style::default().fg(neon_blue())),
        Span::styled(" repeat   ", Style::default().fg(muted_fg())),
        Span::styled("h", Style::default().fg(neon_blue())),
        Span::styled(" theme   ", Style::default().fg(muted_fg())),
        Span::styled("f", Style::default().fg(neon_blue())),
        Span::styled(" folder", Style::default().fg(muted_fg())),
    ]))
    .style(Style::default().bg(panel_bg()));
    f.render_widget(controls, rows[1]);
}

fn render_browser_popup(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let popup = centered_rect(72, 78, area);
    f.render_widget(Clear, popup);

    let glow = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(neon_purple()))
        .style(Style::default().bg(deep_bg()));
    let inner = glow.inner(popup);
    f.render_widget(glow, popup);

    if app.mode == AppMode::PathInput {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(inner);

        let input = Paragraph::new(format!("{}█", app.path_input))
            .block(primary_block(Line::from(vec![Span::styled(
                " Direct Path ",
                Style::default()
                    .fg(neon_blue())
                    .add_modifier(Modifier::BOLD),
            )])))
            .style(Style::default().fg(Color::White));
        f.render_widget(input, chunks[0]);

        let hint = Paragraph::new(vec![
            Line::from("Enter an absolute path or a location starting with ~/"),
            Line::from("Press Enter to load it, or Esc to return to the browser grid."),
        ])
        .block(secondary_block(Line::from(vec![Span::styled(
            " Input Hint ",
            Style::default()
                .fg(neon_pink())
                .add_modifier(Modifier::BOLD),
        )])))
        .style(Style::default().fg(muted_fg()))
        .wrap(Wrap { trim: true });
        f.render_widget(hint, chunks[1]);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(inner);

    let path_label = Paragraph::new(format!(" {}", app.browser_path.display())).style(
        Style::default()
            .fg(neon_blue())
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(path_label, chunks[0]);

    let has_parent = app
        .browser_path
        .parent()
        .map(|parent| parent != app.browser_path.as_path())
        .unwrap_or(false);

    let dir_items: Vec<ListItem> = app
        .browser_entries
        .iter()
        .enumerate()
        .map(|(idx, path)| {
            let label = if idx == 0 && has_parent {
                "[..] Upper Directory".to_string()
            } else {
                path.file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string_lossy().to_string())
            };
            ListItem::new(label)
        })
        .collect();

    let dir_list = List::new(dir_items)
        .block(primary_block(Line::from(vec![Span::styled(
            " Folder Browser ",
            Style::default()
                .fg(neon_blue())
                .add_modifier(Modifier::BOLD),
        )])))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(155, 122, 255))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    f.render_stateful_widget(dir_list, chunks[1], &mut app.browser_state);

    let help =
        Paragraph::new("Enter: open  •  s: set music folder  •  t: type path  •  Esc: close")
            .style(Style::default().fg(muted_fg()));
    f.render_widget(help, chunks[2]);
}

fn render_playlist_name_popup(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let popup = centered_rect(52, 22, area);
    f.render_widget(Clear, popup);

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(neon_purple()))
        .style(Style::default().bg(deep_bg()));
    let inner = outer.inner(popup);
    f.render_widget(outer, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(inner);

    let input = Paragraph::new(format!("{}█", app.playlist_name_input))
        .block(primary_block(Line::from(vec![Span::styled(
            " Playlist Name ",
            Style::default()
                .fg(neon_blue())
                .add_modifier(Modifier::BOLD),
        )])))
        .style(Style::default().fg(Color::White));
    f.render_widget(input, chunks[0]);

    let hint = Paragraph::new("Enter: save  •  Esc: cancel")
        .style(Style::default().fg(muted_fg()))
        .alignment(Alignment::Center);
    f.render_widget(hint, chunks[1]);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = load_config();
    let initial_theme = ThemeMode::from_config(&config.theme);
    ACTIVE_THEME.store(initial_theme.as_index(), Ordering::Relaxed);
    let initial_folder = {
        let path = PathBuf::from(&config.music_folder);
        if path.is_dir() {
            path.canonicalize()
                .unwrap_or(path)
                .to_string_lossy()
                .to_string()
        } else {
            "./music".to_string()
        }
    };

    let songs = load_songs(&initial_folder);
    let songs_are_empty = songs.is_empty();
    let initial_status = if songs_are_empty {
        Some(format!(
            "No mp3 files found in '{}'. Press [f] to select a folder.",
            initial_folder
        ))
    } else {
        Some(format!(
            "{} tracks indexed from {}",
            songs.len(),
            initial_folder
        ))
    };

    let browser_start = PathBuf::from(&initial_folder)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));
    let browser_entries = read_browser_entries(&browser_start);
    let visualizer = Arc::new(Mutex::new(VisualizerState::new(2048, 80)));

    let mut app = App {
        songs,
        directory_counts: Vec::new(),
        artist_counts: Vec::new(),
        album_counts: Vec::new(),
        genre_counts: Vec::new(),
        selected: 0,
        table_state: TableState::default(),
        current_track: None,
        play_start: None,
        song_duration: None,
        is_playing: false,
        paused_elapsed: Duration::ZERO,
        volume: 0.60,
        active_playback: None,
        decoded_track: None,
        pending_decode: None,
        album_art: None,
        album_art_cache: None,
        active_tab: AppTab::Queue,
        music_folder: initial_folder.clone(),
        mode: AppMode::Normal,
        browser_path: browser_start,
        browser_entries,
        browser_state: ListState::default(),
        path_input: String::new(),
        shuffle_enabled: false,
        shuffle_history: Vec::new(),
        play_queue: Vec::new(),
        visualizer,
        repeat_mode: RepeatMode::Off,
        theme_mode: initial_theme,
        status_message: initial_status,
        status_until: Some(
            Instant::now() + Duration::from_secs(if songs_are_empty { 10 } else { 4 }),
        ),
        playlists: load_playlists(),
        playlist_state: ListState::default(),
        playlist_name_input: String::new(),
    };

    if !app.songs.is_empty() {
        app.table_state.select(Some(0));
    }
    refresh_library_metrics(&mut app);

    let (_stream, stream_handle) = OutputStream::try_default()?;
    let sink = Sink::try_new(&stream_handle)?;
    sink.set_volume(app.volume);

    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    loop {
        pump_decode_ready(&mut app);

        if app.current_track.is_some() && app.is_playing && sink.empty() {
            advance_to_next_track(&mut app, &sink);
        }

        if let Some(until) = app.status_until {
            if Instant::now() > until {
                app.status_message = None;
                app.status_until = None;
            }
        }

        let elapsed = current_elapsed(&app);
        let total = app.song_duration.unwrap_or_else(|| {
            current_song(&app)
                .and_then(|song| song.duration)
                .unwrap_or(Duration::ZERO)
        });
        let ratio = if total.as_secs_f64() > 0.0 {
            (elapsed.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0)
        } else {
            0.0
        };

        terminal.draw(|f| {
            let background = Block::default().style(Style::default().bg(deep_bg()));
            f.render_widget(background, f.size());

            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Min(16),
                    Constraint::Length(12),
                    Constraint::Length(4),
                ])
                .split(f.size());

            render_now_playing_bar(f, sections[0], &app);

            let content = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(sections[1]);

            render_art_panel(f, content[0], &mut app);
            if app.mode == AppMode::Normal {
                render_track_panel(f, content[1], &mut app);
            } else {
                let modal_bg = Paragraph::new(vec![
                    Line::from(vec![Span::styled(
                        "Folder mode active",
                        Style::default().fg(neon_blue()).add_modifier(Modifier::BOLD),
                    )]),
                    Line::from("Background queue rendering is reduced for smoother navigation."),
                ])
                .block(primary_block(Line::from(" Browser Focus ")))
                .style(Style::default().fg(muted_fg()))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true });
                f.render_widget(modal_bg, content[1]);
            }
            render_visualizer(f, sections[2], &app);
            render_footer(f, sections[3], &app, elapsed, total, ratio);

            if let Some(message) = &app.status_message {
                let status_area = Rect {
                    x: sections[3].x.saturating_add(2),
                    y: sections[3].y.saturating_sub(1),
                    width: sections[3].width.saturating_sub(4),
                    height: 1,
                };
                let status = Paragraph::new(Line::from(vec![
                    Span::styled(
                        "Status: ",
                        Style::default()
                            .fg(neon_pink())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(message.clone(), Style::default().fg(muted_fg())),
                ]));
                f.render_widget(status, status_area);
            }

            if matches!(app.mode, AppMode::FolderBrowser | AppMode::PathInput) {
                render_browser_popup(f, f.size(), &mut app);
            }
            if app.mode == AppMode::PlaylistNameInput {
                render_playlist_name_popup(f, f.size(), &app);
            }
        })?;

        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(key) = event::read()? {
                match app.mode {
                    AppMode::Normal => match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('f') => {
                            app.browser_path = PathBuf::from(&app.music_folder)
                                .canonicalize()
                                .unwrap_or_else(|_| PathBuf::from("."));
                            app.browser_entries = read_browser_entries(&app.browser_path);
                            app.browser_state.select(Some(0));
                            app.mode = AppMode::FolderBrowser;
                        }
                        KeyCode::Tab => {
                            app.active_tab = next_tab(app.active_tab);
                        }
                        KeyCode::BackTab => {
                            app.active_tab = previous_tab(app.active_tab);
                        }
                        KeyCode::Down => {
                            if app.active_tab == AppTab::Queue && app.selected + 1 < app.songs.len()
                            {
                                app.selected += 1;
                                app.table_state.select(Some(app.selected));
                            } else if app.active_tab == AppTab::Playlists
                                && !app.playlists.is_empty()
                            {
                                let next = (app.playlist_state.selected().unwrap_or(0) + 1)
                                    .min(app.playlists.len() - 1);
                                app.playlist_state.select(Some(next));
                            }
                        }
                        KeyCode::Up => {
                            if app.active_tab == AppTab::Queue && app.selected > 0 {
                                app.selected -= 1;
                                app.table_state.select(Some(app.selected));
                            } else if app.active_tab == AppTab::Playlists {
                                let cur = app.playlist_state.selected().unwrap_or(0);
                                if cur > 0 {
                                    app.playlist_state.select(Some(cur - 1));
                                }
                            }
                        }
                        KeyCode::Left => {
                            seek_relative(&mut app, &sink, -5);
                        }
                        KeyCode::Right => {
                            seek_relative(&mut app, &sink, 5);
                        }
                        KeyCode::Enter => {
                            if app.active_tab == AppTab::Playlists {
                                let idx = app.playlist_state.selected().unwrap_or(0);
                                if !app.playlists.is_empty() {
                                    load_playlist_into_queue(&mut app, &sink, idx, false);
                                }
                            } else {
                                play_selected_song(&mut app, &sink);
                            }
                        }
                        KeyCode::Char('r') => {
                            app.shuffle_enabled = !app.shuffle_enabled;
                            if !app.shuffle_enabled {
                                app.shuffle_history.clear();
                            }
                            app.status_message = Some(if app.shuffle_enabled {
                                "Shuffle mode enabled".to_string()
                            } else {
                                "Shuffle mode disabled".to_string()
                            });
                            app.status_until = Some(Instant::now() + Duration::from_secs(3));
                        }
                        KeyCode::Char('a') => {
                            enqueue_selected_track(&mut app, false);
                        }
                        KeyCode::Char('x') => {
                            enqueue_selected_track(&mut app, true);
                        }
                        KeyCode::Char('s') => {
                            if app.active_tab == AppTab::Playlists {
                                if app.current_track.is_none() && app.play_queue.is_empty() {
                                    app.status_message =
                                        Some("Queue is empty — nothing to save".to_string());
                                    app.status_until =
                                        Some(Instant::now() + Duration::from_secs(3));
                                } else {
                                    app.playlist_name_input = String::new();
                                    app.mode = AppMode::PlaylistNameInput;
                                }
                            }
                        }
                        KeyCode::Char('S') => {
                            if app.active_tab == AppTab::Playlists {
                                let idx = app.playlist_state.selected().unwrap_or(0);
                                if !app.playlists.is_empty() {
                                    load_playlist_into_queue(&mut app, &sink, idx, true);
                                }
                            }
                        }
                        KeyCode::Char('d') => {
                            if app.active_tab == AppTab::Playlists {
                                let idx = app.playlist_state.selected().unwrap_or(0);
                                if let Some(pl) = app.playlists.get(idx) {
                                    let name = pl.name.clone();
                                    if delete_playlist_file(&name) {
                                        app.playlists = load_playlists();
                                        let new_len = app.playlists.len();
                                        let new_sel = if new_len > 0 {
                                            Some(idx.min(new_len - 1))
                                        } else {
                                            None
                                        };
                                        app.playlist_state.select(new_sel);
                                        app.status_message =
                                            Some(format!("Deleted playlist '{}'", name));
                                    } else {
                                        app.status_message =
                                            Some(format!("Failed to delete '{}'", name));
                                    }
                                    app.status_until =
                                        Some(Instant::now() + Duration::from_secs(3));
                                }
                            }
                        }
                        KeyCode::Char('m') => {
                            app.repeat_mode = app.repeat_mode.next();
                            app.status_message = Some(format!(
                                "Repeat mode: {}",
                                app.repeat_mode.label()
                            ));
                            app.status_until = Some(Instant::now() + Duration::from_secs(3));
                        }
                        KeyCode::Char('h') => {
                            app.theme_mode = app.theme_mode.next();
                            ACTIVE_THEME.store(app.theme_mode.as_index(), Ordering::Relaxed);
                            save_config(&Config {
                                music_folder: app.music_folder.clone(),
                                theme: app.theme_mode.config_value().to_string(),
                            });
                            app.status_message = Some(format!(
                                "Theme switched to {}",
                                app.theme_mode.label()
                            ));
                            app.status_until = Some(Instant::now() + Duration::from_secs(3));
                        }
                        KeyCode::Char('n') => {
                            if !app.songs.is_empty() {
                                advance_to_next_track(&mut app, &sink);
                            }
                        }
                        KeyCode::Char('p') => {
                            if !app.songs.is_empty() {
                                if app.shuffle_enabled {
                                    if let Some(previous_idx) = app.shuffle_history.pop() {
                                        play_track_at(&mut app, &sink, previous_idx);
                                    } else {
                                        app.status_message =
                                            Some("Shuffle history is empty".to_string());
                                        app.status_until =
                                            Some(Instant::now() + Duration::from_secs(3));
                                    }
                                } else {
                                    let previous_idx = if app.selected == 0 {
                                        app.songs.len() - 1
                                    } else {
                                        app.selected - 1
                                    };
                                    play_track_at(&mut app, &sink, previous_idx);
                                }
                            }
                        }
                        KeyCode::Char(' ') => {
                            if app.current_track.is_some() {
                                if sink.is_paused() {
                                    sink.play();
                                    if app.active_playback.is_none() {
                                        app.play_start = Some(Instant::now());
                                    }
                                    app.is_playing = true;
                                } else {
                                    sink.pause();
                                    if app.active_playback.is_none() {
                                        if let Some(start) = app.play_start {
                                            app.paused_elapsed += start.elapsed();
                                        }
                                        app.play_start = None;
                                    }
                                    app.is_playing = false;
                                }
                            }
                        }
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            app.volume = (app.volume + 0.1).min(2.0);
                            sink.set_volume(app.volume);
                        }
                        KeyCode::Char('-') => {
                            app.volume = (app.volume - 0.1).max(0.0);
                            sink.set_volume(app.volume);
                        }
                        _ => {}
                    },
                    AppMode::FolderBrowser => match key.code {
                        KeyCode::Esc => {
                            app.mode = AppMode::Normal;
                        }
                        KeyCode::Down => {
                            let len = app.browser_entries.len();
                            if len > 0 {
                                let next =
                                    (app.browser_state.selected().unwrap_or(0) + 1).min(len - 1);
                                app.browser_state.select(Some(next));
                            }
                        }
                        KeyCode::Up => {
                            let current = app.browser_state.selected().unwrap_or(0);
                            if current > 0 {
                                app.browser_state.select(Some(current - 1));
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(idx) = app.browser_state.selected() {
                                if let Some(target) = app.browser_entries.get(idx).cloned() {
                                    if target.is_dir() {
                                        app.browser_path = target.canonicalize().unwrap_or(target);
                                        app.browser_entries =
                                            read_browser_entries(&app.browser_path);
                                        app.browser_state = ListState::default();
                                        app.browser_state.select(Some(0));
                                    }
                                }
                            }
                        }
                        KeyCode::Char('s') => {
                            let new_folder = app.browser_path.to_string_lossy().to_string();
                            let new_songs = load_songs(&new_folder);

                            if new_songs.is_empty() {
                                app.status_message =
                                    Some(format!("No mp3 files found in '{}'", new_folder));
                                app.status_until = Some(Instant::now() + Duration::from_secs(5));
                            } else {
                                save_config(&Config {
                                    music_folder: new_folder.clone(),
                                    theme: app.theme_mode.config_value().to_string(),
                                });
                                sink.stop();
                                app.music_folder = new_folder;
                                app.songs = new_songs;
                                app.selected = 0;
                                app.table_state = TableState::default();
                                app.table_state.select(Some(0));
                                app.current_track = None;
                                app.is_playing = false;
                                app.play_start = None;
                                app.paused_elapsed = Duration::ZERO;
                                app.song_duration = None;
                                app.active_playback = None;
                                app.decoded_track = None;
                                app.pending_decode = None;
                                app.album_art = None;
                                app.album_art_cache = None;
                                app.shuffle_history.clear();
                                app.play_queue.clear();
                                refresh_library_metrics(&mut app);
                                app.status_message =
                                    Some(format!("{} tracks loaded", app.songs.len()));
                                app.status_until = Some(Instant::now() + Duration::from_secs(3));
                                app.mode = AppMode::Normal;
                            }
                        }
                        KeyCode::Char('t') => {
                            app.path_input = app.browser_path.to_string_lossy().to_string();
                            app.mode = AppMode::PathInput;
                        }
                        _ => {}
                    },
                    AppMode::PathInput => match key.code {
                        KeyCode::Esc => {
                            app.mode = AppMode::FolderBrowser;
                        }
                        KeyCode::Backspace => {
                            app.path_input.pop();
                        }
                        KeyCode::Char(c) => {
                            app.path_input.push(c);
                        }
                        KeyCode::Enter => {
                            let raw = app.path_input.trim().to_string();
                            let expanded = if raw.starts_with("~/") {
                                dirs::home_dir()
                                    .map(|home| home.join(&raw[2..]).to_string_lossy().to_string())
                                    .unwrap_or(raw.clone())
                            } else {
                                raw.clone()
                            };

                            let path = PathBuf::from(&expanded);
                            if !path.exists() {
                                app.status_message = Some(format!("'{}' was not found", expanded));
                                app.status_until = Some(Instant::now() + Duration::from_secs(5));
                            } else if !path.is_dir() {
                                app.status_message =
                                    Some("The provided path is not a directory".to_string());
                                app.status_until = Some(Instant::now() + Duration::from_secs(5));
                            } else {
                                let new_folder = path
                                    .canonicalize()
                                    .unwrap_or(path)
                                    .to_string_lossy()
                                    .to_string();
                                let new_songs = load_songs(&new_folder);

                                if new_songs.is_empty() {
                                    app.status_message =
                                        Some(format!("No mp3 files found in '{}'", new_folder));
                                    app.status_until =
                                        Some(Instant::now() + Duration::from_secs(5));
                                } else {
                                    save_config(&Config {
                                        music_folder: new_folder.clone(),
                                        theme: app.theme_mode.config_value().to_string(),
                                    });
                                    sink.stop();
                                    app.music_folder = new_folder;
                                    app.songs = new_songs;
                                    app.selected = 0;
                                    app.table_state = TableState::default();
                                    app.table_state.select(Some(0));
                                    app.current_track = None;
                                    app.is_playing = false;
                                    app.play_start = None;
                                    app.paused_elapsed = Duration::ZERO;
                                    app.song_duration = None;
                                    app.active_playback = None;
                                    app.decoded_track = None;
                                    app.pending_decode = None;
                                    app.album_art = None;
                                    app.album_art_cache = None;
                                    app.shuffle_history.clear();
                                    app.play_queue.clear();
                                    refresh_library_metrics(&mut app);
                                    app.status_message =
                                        Some(format!("{} tracks loaded", app.songs.len()));
                                    app.status_until =
                                        Some(Instant::now() + Duration::from_secs(3));
                                    app.mode = AppMode::Normal;
                                }
                            }
                        }
                        _ => {}
                    },
                    AppMode::PlaylistNameInput => match key.code {
                        KeyCode::Esc => {
                            app.mode = AppMode::Normal;
                        }
                        KeyCode::Backspace => {
                            app.playlist_name_input.pop();
                        }
                        KeyCode::Char(c) => {
                            app.playlist_name_input.push(c);
                        }
                        KeyCode::Enter => {
                            let name = app.playlist_name_input.trim().to_string();
                            if name.is_empty() {
                                app.status_message =
                                    Some("Playlist name cannot be empty".to_string());
                                app.status_until =
                                    Some(Instant::now() + Duration::from_secs(3));
                            } else {
                                save_current_queue_as_playlist(&mut app, &name);
                            }
                            app.mode = AppMode::Normal;
                        }
                        _ => {}
                    },
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    Ok(())
}
