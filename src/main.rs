use std::fs::File; 
use std::io::BufReader; 
use std::time::Duration; 
use std::time::Instant;
use std::io::stdout; 
use std::path::PathBuf;

use ratatui::widgets::ListState; 
use ratatui::style::{Style, Color, Modifier}; l
use ratatui::widgets::Gauge;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Clear;
use ratatui::layout::Rect;

use rodio::{Decoder, OutputStream, Sink} ; 
use walkdir::WalkDir; 


use crossterm::{ 
    event::{self, Event, KeyCode}, 
    execute, 
    terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen}, 
};

use ratatui::{ 
    backend::CrosstermBackend, 
    layout::{Constraint, Direction, Layout}, 
    widgets::{Block, Borders, List, ListItem}, 
    Terminal, 
};

#[derive(serde::Serialize, serde::Deserialize)]
struct Config {
    music_folder: String,
}

#[derive(PartialEq, Clone, Copy)]
enum AppMode {
    Normal,
    FolderBrowser,
    PathInput,
}

struct App {    
    songs: Vec<String>, /
    selected: usize,  
    state: ListState, 
    play_start: Option<Instant>,
    song_duration: Option<Duration>,
    is_playing: bool,
    paused_elapsed: Duration,
    volume: f32, 
    album_art: Option<Vec<u8>>, 

    music_folder: String,
    mode: AppMode,
    browser_path: PathBuf,
    browser_entries: Vec<PathBuf>,
    browser_state: ListState,
    path_input: String,
    status_message: Option<String>,
    status_until: Option<Instant>,
}

fn load_songs(folder: &str) -> Vec<String>{ 
    let mut songs = Vec::new();  

    for entry in WalkDir::new(folder) { 
        let entry = entry.unwrap(); 

        if entry.path().extension().map(|s| s == "mp3").unwrap_or(false) { 
            songs.push(entry.path().display().to_string()); 
        }
    }
    songs 
}
fn get_mp3_duration(path: &str) -> Option<Duration> {
    mp3_duration::from_path(path).ok()
}

fn play_song (path: &str, sink: &Sink) { 
    let file = BufReader::new(File::open(path).unwrap()); 
    let source = Decoder::new(file).unwrap(); 
    
    sink.append(source); 
}

fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}", mins, secs)
}

fn get_album_art(path: &str) -> Option<Vec<u8>>{
    use id3::Tag;
    let tag = Tag::read_from_path(path).ok()?;
    tag.pictures().next().map(|p| p.data.clone())
}

fn render_album_art(data: &[u8], width: u16, height: u16) -> ratatui::text::Text<'static>{
    use image::{GenericImageView, imageops::FilterType};
    let Ok(img) = image::load_from_memory(data) else{
        return ratatui::text::Text::raw("No image");
    };
    let img = img.resize_exact(width as u32, height as u32 * 2, FilterType::Triangle);

    let mut lines = Vec::new();
    for y in (0..img.height()).step_by(2){
        let mut spans = Vec::new();
        for x in 0..img.width(){
            let top = img.get_pixel(x, y);
            let bot = if y + 1 < img.height() { img.get_pixel(x, y + 1)} else { top };
            spans.push(ratatui::text::Span::styled(
                "▄",
                Style::default()
                    .fg(Color::Rgb(bot[0], bot[1], bot[2]))
                    .bg(Color::Rgb(top[0], top[1], top[2])),
            ));
        }
        lines.push(ratatui::text::Line::from(spans));
    }
    ratatui::text::Text::from(lines)
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
    if let Ok(rd) = std::fs::read_dir(path) {
        let mut dirs: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        entries.extend(dirs);
    }
    entries
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
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
    Config { music_folder: "./music".to_string() }
}

fn main() -> Result<(), Box <dyn std::error::Error>> { 
    
    let config = load_config();
    let initial_folder = {
        let p = PathBuf::from(&config.music_folder);
        if p.is_dir() {
            p.canonicalize().unwrap_or(p).to_string_lossy().to_string()
        } else {
            "./music".to_string()
        }
    };

    let songs = load_songs(&initial_folder);

    let songs_is_empty = songs.is_empty();


    let initial_status: Option<String> = if songs_is_empty {
        Some(format!(" '{}' klasöründe mp3 yok. [f] ile klasör seç.", initial_folder))
    } else {
        None
    };

    let browser_start = PathBuf::from(&initial_folder)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));
    let browser_entries = read_browser_entries(&browser_start);

    let mut app = App { 
        songs, 
        selected: 0, 
        state: ListState::default(), 
        play_start: None,
        song_duration: None,
        is_playing: false,
        paused_elapsed: Duration::ZERO,
        volume: 1.0, 
        album_art: None, 

        music_folder: initial_folder.clone(),
        mode: AppMode::Normal,
        browser_path: browser_start,
        browser_entries,
        browser_state: ListState::default(),
        path_input: String::new(),
        status_message: initial_status,
        status_until: if songs_is_empty {
            Some(Instant::now() + Duration::from_secs(10))
        } else {
            None
        },
    };

    if !app.songs.is_empty() {
        app.state.select(Some(0));
    }

    app.state.select(Some(0)); 

    let (_stream, stream_handle) = OutputStream::try_default()?; 
    let sink = Sink::try_new(&stream_handle)?; 

    enable_raw_mode()?; 
    execute!(stdout(), EnterAlternateScreen)?; 

    let backend = CrosstermBackend::new(stdout()); 
    let mut terminal = Terminal::new(backend)?; 

    loop { 
        if let Some(until) = app.status_until {
            if Instant::now() > until {
                app.status_message = None;
                app.status_until = None;
            }
        }
        
        let elapsed = if app.is_playing {
            if let Some(start) = app.play_start {
                app.paused_elapsed + start.elapsed()
            } else {
                Duration::ZERO
            }
        } else{
            app.paused_elapsed
        };

        let total = app.song_duration.unwrap_or(Duration::ZERO);
        let ratio = if total.as_secs() > 0 {
            (elapsed.as_secs_f64() / total.as_secs_f64()).min(1.0)
        } else {
            0.0
        };

        let time_label = if total.as_secs() > 0 {
            format! (" {} / {} ", format_duration(elapsed), format_duration(total))
        } else {
            " --:-- / --:-- ".to_string()
        };
        
        
        
        terminal.draw(|f| { 

            let main_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(1), 
                    Constraint::Length(14)])
                .split(f.size());

            let bottom_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(28), Constraint::Min(0)])
                .split(main_chunks[2]);
            
            let control_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(3), Constraint::Length(3)])
                .split(bottom_chunks[1]);
            
            let items: Vec<ListItem> = app
                .songs
                .iter()
                .map(|s|{
                    let name = std::path::Path::new(s)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| s.clone());
                    ListItem::new(name)
                })
                .collect();

            let art_block = Block::default().title(" Album ").borders(Borders::ALL);
            if let Some(art_data) = &app.album_art {
                let inner = art_block.inner(bottom_chunks[0]);
                let art_text = render_album_art(art_data, inner.width, inner.height);
                let art_widget = Paragraph::new(art_text).block(art_block);
                f.render_widget(art_widget, bottom_chunks[0]);
            } else {
                let placeholder = Paragraph::new("\n\n     ♪")
                    .block(art_block);
                f.render_widget (placeholder, bottom_chunks[0]);
            }

            let list = List::new(items) 
                .block(Block::default()
                    .title(format!(
                        "MP3 Player  [↑↓] Seç  [Enter] Oynat  [Space] Dur  [q] Çık  [+/-] Ses  [f] Klasör  │  📂 {}",
                        std::path::Path::new(&app.music_folder)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| app.music_folder.clone())
                    ))
                    .borders(Borders::ALL))
                .highlight_style(
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                )
                .highlight_symbol(">> ");
            f.render_stateful_widget(list, main_chunks[0], &mut app.state);

            let status_text = app.status_message.clone()
                .unwrap_or_else(|| format!(" 📂 {}", app.music_folder));
            let status_style = if app.status_message.is_some() {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            f.render_widget(
                Paragraph::new(status_text).style(status_style),
                main_chunks[1],
            );

            let gauge = Gauge::default()
                .block(
                    Block::default()
                        .title(time_label)
                        .borders(Borders::ALL),
                )
                .gauge_style(
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::DarkGray),
                )
                .ratio(ratio);
            f.render_widget(gauge, control_chunks[1]);

            let vol_gauge = Gauge::default()
                .block(Block::default().title(format!(" Ses: {:.0}% ", app.volume * 100.0)).borders(Borders::ALL))
                .gauge_style(Style::default().fg(Color::Green).bg(Color::DarkGray))
                .ratio((app.volume / 2.0) as f64);
            f.render_widget(vol_gauge, control_chunks[2]);

            if app.mode == AppMode::FolderBrowser {
                let popup = centered_rect(72, 78, f.size());
                f.render_widget(Clear, popup); 

                if app.mode == AppMode::PathInput{
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(3), Constraint::Min(0)])
                        .split(popup);

                        let input_paragraph = Paragraph::new(format!("{}_", app.path_input))
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .title(" Klasör Yolu Yaz ")
                                    .border_style(Style::default().fg(Color::Cyan)));
                        f.render_widget(input_paragraph, chunks[0]);

                        let hint = Paragraph::new(" Mutlak yol veya ~ ile başlayan ev dizini desteklenir. [Enter] ile onayla, [Esc] ile iptal ")
                            .block(Block::default().borders(Borders::ALL).title(" İpucu ") )
                            .style(Style::default().fg(Color::DarkGray));
                        f.render_widget(hint, chunks[1]);
                } else {
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(2), Constraint::Min(0), Constraint::Length(2)])
                        .split(popup);

                        let path_label = Paragraph::new(format!(" {}", app.browser_path.display()))
                        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
                        f.render_widget(path_label, chunks[0]);

                        let has_parent = app.browser_path.parent()
                        .map(|p| p != app.browser_path.as_path())
                        .unwrap_or(false);

                        let dir_items: Vec<ListItem> = app.browser_entries
                            .iter()
                            .enumerate()
                            .map(|(i, p)| {
                                let label = if i == 0 && has_parent {
                                    "Üst Dizin [..]".to_string()
                                } else {
                                    p.file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_else(|| p.to_string_lossy().to_string())
                                };
                                ListItem::new(label)
                            })
                            .collect();

                        let dir_list = List::new(dir_items)
                        .block(Block::default()
                        .title(" Klasör Seç  [↑↓] Gezin  [Enter] Gir  [s] Seç  [t] Yol Yaz  [Esc] İptal ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan)))
                        .highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD))
                        .highlight_symbol("> ");
                        f.render_stateful_widget(dir_list, chunks[1], &mut app.browser_state);

                        let help = Paragraph::new("  [s] Bu dizini müzik klasörü olarak seç")
                        .style(Style::default().fg(Color::Green));
                        f.render_widget(help, chunks[2]);
                    
                }
            }

        })?; 

        if event::poll(Duration::from_millis(200))? { 
            if let Event::Key(key) = event::read()? {
                match app.mode {
                    AppMode::Normal => match key.code {
                        KeyCode::Char('q') => break,

                        KeyCode::Char('f') =>{
                            app.browser_path = PathBuf::from(&app.music_folder)
                                .canonicalize()
                                .unwrap_or_else(|_| PathBuf::from("."));
                            app.browser_entries = read_browser_entries(&app.browser_path);
                            app.browser_state.select(Some(0));
                            app.mode = AppMode::FolderBrowser;
                        }

                        KeyCode::Down => {
                            if app.selected + 1 < app.songs.len() {
                                app.selected += 1;
                                app.state.select(Some(app.selected));
                            }
                        }
                        KeyCode::Up => {
                            if app.selected > 0 {
                                app.selected -= 1;
                                app.state.select(Some(app.selected));
                            }
                        }

                        KeyCode::Enter => {
                            if !app.songs.is_empty(){
                                sink.stop();
                                play_song(&app.songs[app.selected], &sink);
                                app.album_art = get_album_art(&app.songs[app.selected]);
                                app.song_duration = get_mp3_duration(&app.songs[app.selected]);
                                app.play_start = Some(Instant::now());
                                app.is_playing = true;
                                app.paused_elapsed = Duration::ZERO;
                            }
                        }

                        KeyCode::Char(' ') => {
                            if sink.is_paused() {
                                sink.play();
                                app.play_start = Some(Instant::now());
                                app.is_playing = true;
                            } else {
                                sink.pause();
                                if let Some(start) = app.play_start {
                                    app.paused_elapsed += start.elapsed();
                                }
                                app.play_start = None;
                                app.is_playing = false;
                            }
                        }

                        KeyCode::Char('+') => {
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
                        KeyCode::Esc => { app.mode = AppMode::Normal; }

                        KeyCode::Down => {
                            let len = app.browser_entries.len();
                            if len > 0 {
                                let next = (app.browser_state.selected().unwrap_or(0) + 1).min(len -1);
                                app.browser_state.select(Some(next));
                        }
                    }
                        KeyCode::Up => {
                            let cur = app.browser_state.selected().unwrap_or(0);
                            if cur > 0 { app.browser_state.select(Some(cur -1)); }
                        }

                        KeyCode::Enter => {
                            if let Some(idx) = app.browser_state.selected(){
                                if let Some(target) = app.browser_entries.get(idx).cloned(){
                                    if target.is_dir(){
                                        app.browser_path = target.canonicalize().unwrap_or(target);
                                        app.browser_entries = read_browser_entries(&app.browser_path);
                                        app.browser_state = ListState::default();
                                        app.browser_state.select(Some(0));
                                    }
                                }
                            }
                        }

                        KeyCode::Char('s') => {
                            let new_folder = app.browser_path.to_string_lossy().to_string();
                            let new_songs = load_songs(&new_folder);
                            if new_songs.is_empty()  {
                                app.status_message = Some(format!(
                                    " '{}' klasöründe mp3 yok.",
                                    app.browser_path.file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_default()
                                ));
                                app.status_until = Some(Instant::now() + Duration::from_secs(5));
                            } else {
                                save_config(&Config { music_folder: new_folder.clone() });
                                sink.stop();
                                app.music_folder = new_folder;
                                app.songs = new_songs;
                                app.selected = 0;
                                app.state = ListState::default();
                                app.state.select(Some(0));
                                app.is_playing = false;
                                app.album_art = None;
                                app.status_message = Some(format!(" {} şarkı yüklendi", app.songs.len()));
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
                        KeyCode::Esc => { app.mode = AppMode::FolderBrowser; }
                        KeyCode::Backspace => { app.path_input.pop(); }
                        KeyCode::Char(c) => { app.path_input.push(c); }
                        KeyCode::Enter => {
                            let raw = app.path_input.trim().to_string();
                            let expanded = if raw.starts_with("~/"){
                                dirs::home_dir()
                                    .map(|h| h.join(&raw[2..]).to_string_lossy().to_string())
                                    .unwrap_or(raw.clone())

                            } else {
                                raw.clone()
                            };

                            let path = PathBuf::from(&expanded);
                            if !path.exists() {
                                app.status_message = Some(format!(" '{}' bulunamadı.", expanded));
                                app.status_until = Some(Instant::now() + Duration::from_secs(5));
                            }else if !path.is_dir(){
                                app.status_message = Some(" Belirtilen yol bir dizin değil.".to_string());
                                app.status_until = Some(Instant::now() + Duration::from_secs(5));
                            } else{
                                let new_folder = path.canonicalize().unwrap_or(path)
                                    .to_string_lossy()
                                    .to_string();
                                let new_songs = load_songs(&new_folder);
                                if new_songs.is_empty()  {
                                    app.status_message = Some(format!(" Bu klasörde mp3 yok: {}", new_folder));
                                    app.status_until = Some(Instant::now() + Duration::from_secs(5));
                                } else{
                                    save_config(&Config { music_folder: new_folder.clone()});
                                    sink.stop();
                                    app.music_folder = new_folder;
                                    app.songs = new_songs;
                                    app.selected = 0;
                                    app.state = ListState::default();
                                    app.state.select(Some(0));
                                    app.is_playing = false;
                                    app.play_start = None;
                                    app.paused_elapsed = Duration::ZERO;
                                    app.song_duration = None;
                                    app.album_art = None;
                                    app.status_message = Some(format!(" {} şarkı yüklendi", app.songs.len()));
                                    app.status_until = Some(Instant::now() + Duration::from_secs(3));
                                    app.mode = AppMode::Normal;
                                }
                            }
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