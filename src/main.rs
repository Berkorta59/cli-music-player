use std::fs::File; // standart dosya sistemi modülünden file sınıfı import edildi
use std::io::BufReader; // dosyaları daha verimli okumak için kullanılan bir sınıf
use std::time::Duration; // süre ölçümü için kullanılan bir sınıf

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

use ratatui::{ // terminal arayüzü oluşturmak için kullanılan bir kütüphane
    backend::CrosstermBackend, // crossterm ile uyumlu bir backend
    layout::{Constraint, Direction, Layout}, // terminal düzeni için kullanılan modül
    widgets::{Block, Borders, List, ListItem}, // terminaldeki widget'ları oluşturmak için kullanılan modül
    Terminal, // terminali oluşturmak için kullanılan modül
};

use rodio::{Decoder, OutputStream, Sink} ; // ses oynatma için kullanılan bir kütüphane

use walkdir::WalkDir; // dosya sisteminde gezinmek için kullanılan bir kütüphane

use std::io::stdout; // standart çıktı için kullanılan modül

struct App {    // uygulama sınıfı
    songs: Vec<String>, // şarkıların dosya yollarını tutan bir vektör
    selected: usize,  // seçili şarkının indeksini tutan bir değişken
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

fn play_song (path: &str, sink: &Sink) { // belirtilen şarkıyı oynatan bir fonksiyon
    let file = BufReader::new(File::open(path).unwrap()); // şarkı dosyasını açmak için kullanılan bir kod
    let source = Decoder::new(file).unwrap(); // şarkı dosyasını decode etmek için kullanılan bir kod
    
    sink.append(source); // şarkıyı oynatmak için kullanılan bir kod
}

fn main() -> Result<(), Box <dyn std::error::Error>> {
    let songs = load_songs("./music");
    
    if songs.is_empty(){
        println!("music klasörüne mp3 koy");
        return Ok(());
    }

    let mut app = App {
        songs,
        selected: 0,
    };

    let (_stream, stream_handle) = OutputStream::try_default()?;
    let sink = Sink::try_new(&stream_handle)?;

    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    loop {
        terminal.draw(|f| {

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(100)])
                .split(f.size());
            
            let items: Vec<ListItem> = app
                .songs
                .iter()
                .map(|s| ListItem::new(s.clone()))
                .collect();

            let list = List::new(items)
                .block(Block::default().title("MP3 Player").borders(Borders::ALL));

            f.render_widget(list, chunks[0]);
        })?;

        if event::poll(Duration::from_millis(200))? {
            
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => break,

                    KeyCode::Down => {
                        if app.selected + 1 < app.songs.len(){
                            app.selected += 1;
                        }
                    }

                    KeyCode::Up => {
                        if app.selected > 0 {
                            app.selected -= 1;
                        }
                    }

                    KeyCode::Enter => {
                        sink.stop();
                        play_song(&app.songs[app.selected], &sink);
                    }

                    KeyCode::Char(' ') => {
                        if sink.is_paused(){
                            sink.play();
                        } else {
                            sink.pause();
                        }
                    }

                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;

    Ok(())
}