use std::fs::File; // standart dosya sistemi modülünden file sınıfı import edildi
use std::io::BufReader; // dosyaları daha verimli okumak için kullanılan bir sınıf
use std::time::Duration; // süre ölçümü için kullanılan bir sınıf
use ratatui::widgets::ListState; // terminal arayüzünde liste widget'ının durumunu tutmak için kullanılan bir sınıf
use ratatui::style::{Style, Color, Modifier}; // terminal arayüzünde stil ve renkler için kullanılan modül
use std::time::Instant;
use ratatui::widgets::Gauge;
use rodio::{Decoder, OutputStream, Sink} ; // ses oynatma için kullanılan bir kütüphane
use walkdir::WalkDir; // dosya sisteminde gezinmek için kullanılan bir kütüphane
use std::io::stdout; // standart çıktı için kullanılan modül


use crossterm::{ // terminal kontrolü için kullanılan bir kütüphane
    event::{self, Event, KeyCode}, // terminaldeki olayları dinlemek için kullanılan modül
    execute, // terminal komutlarını çalıştırmak için kullanılan modül
    terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen}, // terminal modlarını kontrol etmek için kullanılan modül
};

use ratatui::{ // terminal arayüzü oluşturmak için kullanılan bir kütüphane
    backend::CrosstermBackend, // crossterm ile uyumlu bir backend
    layout::{Constraint, Direction, Layout}, // terminal düzeni için kullanılan modül
    widgets::{Block, Borders, List, ListItem}, // terminaldeki widget'ları oluşturmak için kullanılan modül
    Terminal, // terminali oluşturmak için kullanılan modül
};

struct App {    // uygulama sınıfı
    songs: Vec<String>, // şarkıların dosya yollarını tutan bir vektör
    selected: usize,  // seçili şarkının indeksini tutan bir değişken
    state: ListState, // terminal arayüzünde liste widget'ının durumunu tutan bir değişken
    play_start: Option<Instant>,
    song_duration: Option<Duration>,
    is_playing: bool,
    paused_elapsed: Duration,
    volume: f32, // ses seviyesini tutan bir değişken
}

fn load_songs(folder: &str) -> Vec<String>{ // belirtilen klasördeki mp3 dosyalarını yükleyen bir fonksiyon
    let mut songs = Vec::new();  // şarkıların dosya yollarını tutan bir vektör

    for entry in WalkDir::new(folder) { // klasördeki dosyaları gezmek için kullanılan bir döngü
        let entry = entry.unwrap(); // dosya gezme işlemi sırasında oluşabilecek hataları yakalamak için unwrap kullanıldı

        if entry.path().extension().map(|s| s == "mp3").unwrap_or(false) { // dosyanın uzantısı mp3 ise
            songs.push(entry.path().display().to_string()); // dosya yolunu string olarak vektöre eklemek için kullanılan bir kod
        }
    }
    songs // şarkıların dosya yollarını içeren vektör döndürülür
}
fn get_mp3_duration(path: &str) -> Option<Duration> {
    mp3_duration::from_path(path).ok()
}

fn play_song (path: &str, sink: &Sink) { // belirtilen şarkıyı oynatan bir fonksiyon
    let file = BufReader::new(File::open(path).unwrap()); // şarkı dosyasını açmak için kullanılan bir kod
    let source = Decoder::new(file).unwrap(); // şarkı dosyasını decode etmek için kullanılan bir kod
    
    sink.append(source); // şarkıyı oynatmak için kullanılan bir kod
}

fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}", mins, secs)
}

fn main() -> Result<(), Box <dyn std::error::Error>> { // ana fonksiyon, uygulamanın giriş noktası
    let songs = load_songs("./music"); // music klasöründe bulunan mp3 dosyalarını yükle
    
    if songs.is_empty(){ // eğer şarkı bulunamazsa kullanıcıya bilgi vermek için kullanılan bir kod
        println!("music klasörüne mp3 koy"); // kullanıcıya bilgi vermek için kullanılan bir kod
        return Ok(()); // uygulamayı sonlandırmak için kullanılan bir kod
    }

    let mut app = App { // uygulama sınıfının bir örneği oluşturulur
        songs, // şarkıların dosya yollarını içeren vektör
        selected: 0, // başlangıçta seçili şarkının indeksini 0 olarak ayarlamak için kullanılan bir kod
        state: ListState::default(), // liste widget'ının durumunu varsayılan olarak ayarlamak için kullanılan bir kod
        play_start: None,
        song_duration: None,
        is_playing: false,
        paused_elapsed: Duration::ZERO,
        volume: 1.0, // başlangıçta ses seviyesini 1.0 (maksimum) olarak ayarlamak için kullanılan bir kod
    };

    app.state.select(Some(0)); // başlangıçta ilk şarkıyı seçili olarak göstermek için kullanılan bir kod

    let (_stream, stream_handle) = OutputStream::try_default()?; // ses çıkışını başlatmak için kullanılan bir kod
    let sink = Sink::try_new(&stream_handle)?; // ses çıkışını kontrol etmek için kullanılan bir kod

    enable_raw_mode()?; // terminali ham moduna geçirmek için kullanılan bir kod
    execute!(stdout(), EnterAlternateScreen)?; // alternatif ekran moduna geçmek için kullanılan bir kod

    let backend = CrosstermBackend::new(stdout()); // crossterm backend'i oluşturmak için kullanılan bir kod
    let mut terminal = Terminal::new(backend)?; // terminali oluşturmak için kullanılan bir kod

    loop { // ana döngü, kullanıcı etkileşimlerini dinlemek ve terminal arayüzünü güncellemek için kullanılır
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
        
        
        
        terminal.draw(|f| { // terminal arayüzü güncellemek için kullanılan bir kod

            let chunks = Layout::default() // terminal düzenini tanımlamak için kullanılan bir kod
                .direction(Direction::Vertical) // dikey olarak oluşturmak için kullanılan bir kod
                .constraints([
                    Constraint::Min(3), 
                    Constraint::Length(3),
                    Constraint::Length(3), 
                ]) 
                .split(f.size()); // terminal alanını bölmek için kullanılan bir kod
            
            let items: Vec<ListItem> = app // şarkıların dosya yollarını list item'lara dönüştürmek için kullanılan bir kod
                .songs // şarkıların dosya yollarını içeren vektör
                .iter() // şarkıların dosya yollarını iterasyon yapmak için kullanılan bir kod
                .map(|s|{
                    let name = std::path::Path::new(s)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| s.clone());
                    ListItem::new(name)
                })
                .collect(); // dosya yollarını bir vektörde toplamak için kullanılan bir kod

            let list = List::new(items) // list widget'ını oluşturmak için kullanılan bir kod
                .block(Block::default().title("MP3 Player  [↑↓] Seç  [Enter] Oynat  [Space] Duraklat  [q] Çık [+/-] Ses").borders(Borders::ALL)) // list widget'ının başlığını ve kenarlıklarını ayarlamak için kullanılan bir kod
                .highlight_style( // seçili şarkının stilini ayarlamak için kullanılan bir kod
                    Style::default() // varsayılan stil
                        .bg(Color::Blue) // arka plan rengini mavi yapmak için kullanılan bir kod
                        .fg(Color::White) // metin rengini beyaz yapmak için kullanılan bir kod
                        .add_modifier(Modifier::BOLD) // metni kalın yapmak için kullanılan bir kod
                )
                .highlight_symbol(">> "); // seçili şarkının başına ">> " sembolü eklemek için kullanılan bir kod

            f.render_stateful_widget(list, chunks[0], &mut app.state); 

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
            
            f.render_widget(gauge, chunks[1]);

            let vol_gauge = Gauge::default()
                .block(Block::default().title(format!(" Ses: {:.0}% ", app.volume * 100.0)).borders(Borders::ALL))
                .gauge_style(Style::default().fg(Color::Green).bg(Color::DarkGray))
                .ratio((app.volume / 2.0) as f64);
            f.render_widget(vol_gauge, chunks[2]);

        })?; // terminal arayüzünü güncellemek için kullanılan bir kod

        if event::poll(Duration::from_millis(200))? { // kullanıcı etkileşimlerini dinlemek için kullanılan bir kod
            
            if let Event::Key(key) = event::read()? { // klavye olaylarını dinlemek için kullanılan bir kod
                match key.code { // klavye tuşlarına göre işlemler yapmak için kullanılan bir kod
                    KeyCode::Char('q') => break, // 'q' tuşuna basıldığında döngüyü kırmak için kullanılan bir kod

                    KeyCode::Down => { // aşağı ok tuşuna basıldığında seçili şarkıyı değiştirmek için kullanılan bir kod
                        if app.selected + 1 < app.songs.len(){ // seçili şarkının indeksini artırmak için kullanılan bir kod
                            app.selected += 1; 
                            app.state.select(Some(app.selected));
                        }
                    }

                    KeyCode::Up => { // yukarı ok tuşuna basıldığında seçili şarkıyı değiştirmek için kullanılan bir kod
                        if app.selected > 0 { // seçili şarkının indeksini azaltmak için kullanılan bir kod
                            app.selected -= 1; 
                            app.state.select(Some(app.selected));
                        }
                    }

                    KeyCode::Enter => { // enter tuşuna basıldığında seçili şarkıyı oynatmak için kullanılan bir kod
                        sink.stop(); // mevcut şarkıyı durdurmak için kullanılan bir kod
                        play_song(&app.songs[app.selected], &sink); // seçili şarkıyı oynatmak için kullanılan bir kod

                        app.song_duration = get_mp3_duration(&app.songs[app.selected]);
                        app.play_start = Some(Instant::now());
                        app.paused_elapsed = Duration::ZERO;
                        app.is_playing = true;
                    }

                    KeyCode::Char(' ') => { // boşluk tuşuna basıldığında şarkıyı duraklatmak veya devam ettirmek için kullanılan bir kod
                        if sink.is_paused(){ // şarkı duraklatılmışsa devam ettirmek için kullanılan bir kod
                            sink.play(); // şarkıyı devam ettirmek için kullanılan bir kod

                            app.play_start = Some(Instant::now());
                            app.is_playing = true;
                        } else { 
                            if let Some(start) = app.play_start{
                                app.paused_elapsed += start.elapsed();
                            }

                            app.play_start = None;
                            app.is_playing = false;
                            sink.pause(); // şarkıyı duraklatmak için kullanılan bir kod
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

                    _ => {} // diğer tuşlara basıldığında herhangi bir işlem yapmamak için kullanılan bir kod
                }
            }
        }
    }

    disable_raw_mode()?; // terminali normal moduna geri döndürmek için kullanılan bir kod
    execute!(stdout(), LeaveAlternateScreen)?; // alternatif ekran modundan çıkmak için kullanılan bir kod

    Ok(()) // uygulamanın başarılı bir şekilde tamamlandığını belirtmek için kullanılan bir kod
}