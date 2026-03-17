use std::fs::File; // standart dosya sistemi modülünden file sınıfı import edildi
use std::io::BufReader; // dosyaları daha verimli okumak için kullanılan bir sınıf
use std::time::Duration; // süre ölçümü için kullanılan bir sınıf

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

fn main() -> Result<(), Box <dyn std::error::Error>> { // ana fonksiyon, uygulamanın giriş noktası
    let songs = load_songs("./music"); // music klasöründe bulunan mp3 dosyalarını yükle
    
    if songs.is_empty(){ // eğer şarkı bulunamazsa kullanıcıya bilgi vermek için kullanılan bir kod
        println!("music klasörüne mp3 koy"); // kullanıcıya bilgi vermek için kullanılan bir kod
        return Ok(()); // uygulamayı sonlandırmak için kullanılan bir kod
    }

    let mut app = App { // uygulama sınıfının bir örneği oluşturulur
        songs, // şarkıların dosya yollarını içeren vektör
        selected: 0, // başlangıçta seçili şarkının indeksini 0 olarak ayarlamak için kullanılan bir kod
    };

    let (_stream, stream_handle) = OutputStream::try_default()?; // ses çıkışını başlatmak için kullanılan bir kod
    let sink = Sink::try_new(&stream_handle)?; // ses çıkışını kontrol etmek için kullanılan bir kod

    enable_raw_mode()?; // terminali ham moduna geçirmek için kullanılan bir kod
    execute!(stdout(), EnterAlternateScreen)?; // alternatif ekran moduna geçmek için kullanılan bir kod

    let backend = CrosstermBackend::new(stdout()); // crossterm backend'i oluşturmak için kullanılan bir kod
    let mut terminal = Terminal::new(backend)?; // terminali oluşturmak için kullanılan bir kod

    loop { // ana döngü, kullanıcı etkileşimlerini dinlemek ve terminal arayüzünü güncellemek için kullanılır
        terminal.draw(|f| { // terminal arayüzü güncellemek için kullanılan bir kod

            let chunks = Layout::default() // terminal düzenini tanımlamak için kullanılan bir kod
                .direction(Direction::Vertical) // dikey olarak oluşturmak için kullanılan bir kod
                .constraints([Constraint::Percentage(100)]) // terminal boyutunu ayarlamak için kullanılan bir kod
                .split(f.size()); // terminal alanını bölmek için kullanılan bir kod
            
            let items: Vec<ListItem> = app // şarkıların dosya yollarını list item'lara dönüştürmek için kullanılan bir kod
                .songs // şarkıların dosya yollarını içeren vektör
                .iter() // şarkıların dosya yollarını iterasyon yapmak için kullanılan bir kod
                .map(|s| ListItem::new(s.clone())) // her bir şarkı dosya yolunu ListItem'a dönüştürmek için kullanılan bir kod
                .collect(); // dosya yollarını bir vektörde toplamak için kullanılan bir kod

            let list = List::new(items) // list widget'ını oluşturmak için kullanılan bir kod
                .block(Block::default().title("MP3 Player").borders(Borders::ALL)); // list widget'ının başlığını ve kenarlıklarını ayarlamak için kullanılan bir kod

            f.render_widget(list, chunks[0]); // list widget'ını terminaldeki belirli bir alana render etmek için kullanılan bir kod
        })?; // terminal arayüzünü güncellemek için kullanılan bir kod

        if event::poll(Duration::from_millis(200))? { // kullanıcı etkileşimlerini dinlemek için kullanılan bir kod
            
            if let Event::Key(key) = event::read()? { // klavye olaylarını dinlemek için kullanılan bir kod
                match key.code { // klavye tuşlarına göre işlemler yapmak için kullanılan bir kod
                    KeyCode::Char('q') => break, // 'q' tuşuna basıldığında döngüyü kırmak için kullanılan bir kod

                    KeyCode::Down => { // aşağı ok tuşuna basıldığında seçili şarkıyı değiştirmek için kullanılan bir kod
                        if app.selected + 1 < app.songs.len(){ // seçili şarkının indeksini artırmak için kullanılan bir kod
                            app.selected += 1; 
                        }
                    }

                    KeyCode::Up => { // yukarı ok tuşuna basıldığında seçili şarkıyı değiştirmek için kullanılan bir kod
                        if app.selected > 0 { // seçili şarkının indeksini azaltmak için kullanılan bir kod
                            app.selected -= 1; 
                        }
                    }

                    KeyCode::Enter => { // enter tuşuna basıldığında seçili şarkıyı oynatmak için kullanılan bir kod
                        sink.stop(); // mevcut şarkıyı durdurmak için kullanılan bir kod
                        play_song(&app.songs[app.selected], &sink); // seçili şarkıyı oynatmak için kullanılan bir kod
                    }

                    KeyCode::Char(' ') => { // boşluk tuşuna basıldığında şarkıyı duraklatmak veya devam ettirmek için kullanılan bir kod
                        if sink.is_paused(){ // şarkı duraklatılmışsa devam ettirmek için kullanılan bir kod
                            sink.play(); // şarkıyı devam ettirmek için kullanılan bir kod
                        } else { 
                            sink.pause(); // şarkıyı duraklatmak için kullanılan bir kod
                        }
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