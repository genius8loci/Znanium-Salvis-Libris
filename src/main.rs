// salvis — загрузчик книг с znanium.ru, портированный с Python (salvis.py) на Rust.
// Использует playwright-rs 0.15 для управления headless-браузером.

use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::Parser;
use image::{DynamicImage, GenericImage};
use playwright_rs::{install_browsers, protocol::{Cookie, Page, Playwright}};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Аргументы командной строки: логин и пароль от аккаунта znanium.ru
#[derive(Parser, Debug)]
#[command(name = "salvis", about = "Znanium.ru downloader")]
struct Args {
    /// Логин от учётной записи
    login: String,
    /// Пароль от учётной записи
    password: String,
}

const COOKIES_PATH: &str = "cookies.json";
const LINKS_PATH: &str = "links.txt";
const PROFILE_URL: &str = "https://znanium.ru/user/my-profile";
const LOGIN_URL: &str = "https://znanium.ru/site/login";

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    install_browsers(Some(&["firefox"]))
        .await
        .context("не удалось установить Firefox")?;

    let playwright = Playwright::launch()
        .await
        .context("не удалось запустить playwright-rs")?;
    let browser = playwright
        .firefox()
        .launch()
        .await
        .context("не удалось запустить Firefox")?;

    let context = browser
        .new_context()
        .await
        .context("не удалось создать контекст браузера")?;

    // Пустая страница, чтобы браузер не закрылся при закрытии рабочих вкладок
    let _keepalive_page = context.new_page().await?;

    auth_cookies(&context, &args.login, &args.password).await?;

    let links = read_links(LINKS_PATH)?;

    for link in links {
        if let Err(err) = load_book(&context, &link).await {
            eprintln!("Ошибка при обработке {link}: {err:#}");
        }
    }

    browser.close().await?;
    Ok(())
}

/// Читает ссылки на книги из текстового файла, пропуская пустые строки.
fn read_links(path: &str) -> Result<Vec<String>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("не удалось прочитать {path}"))?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect())
}

/// Проверка и восстановление сессии: подгружает cookies.json либо логинится заново.
async fn auth_cookies(
    context: &playwright_rs::protocol::BrowserContext,
    login: &str,
    password: &str,
) -> Result<()> {
    if let Ok(raw) = fs::read_to_string(COOKIES_PATH) {
        let cookies: Vec<Cookie> =
            serde_json::from_str(&raw).context("повреждённый cookies.json")?;
        context.add_cookies(&cookies).await?;

        let auth_page = context.new_page().await?;
        auth_page.goto(PROFILE_URL, None).await?;

        if auth_page.url() == PROFILE_URL {
            auth_page.close().await?;
            return Ok(());
        }

        if auth_page.url() == LOGIN_URL {
            perform_login(&auth_page, login, password).await?;
            save_cookies(context).await?;
        }
        auth_page.close().await?;
        return Ok(());
    }

    let auth_page = context.new_page().await?;
    auth_page.goto(PROFILE_URL, None).await?;

    if auth_page.url() == LOGIN_URL {
        perform_login(&auth_page, login, password).await?;
        save_cookies(context).await?;
    }
    auth_page.close().await?;
    Ok(())
}

/// Заполняет форму авторизации и дожидается перехода в личный кабинет.
async fn perform_login(page: &Page, login: &str, password: &str) -> Result<()> {
    page.get_by_label("Логин или Email", false)
        .fill(login, None)
        .await?;
    page.get_by_label("Пароль", false)
        .fill(password, None)
        .await?;
    page.get_by_role(
        playwright_rs::protocol::AriaRole::Button,
        Some(playwright_rs::protocol::GetByRoleOptions::default().name("Вход")),
    )
    .click(None)
    .await?;
    page.wait_for_url(PROFILE_URL, None).await?;
    Ok(())
}

/// Сохраняет актуальные cookies контекста в файл.
async fn save_cookies(context: &playwright_rs::protocol::BrowserContext) -> Result<()> {
    let cookies = context.cookies(None).await?;
    let json = serde_json::to_string(&cookies)?;
    fs::write(COOKIES_PATH, json)?;
    Ok(())
}

/// Разбирает XML-ответ вида <bookpages><sliceN>data:image/png;base64,...</sliceN>...</bookpages>
/// и возвращает срезы страницы в порядке их номеров.
fn parse_slices(xml_bytes: &[u8]) -> Result<Vec<(u32, Vec<u8>)>> {
    let mut reader = Reader::from_reader(xml_bytes);
    reader.config_mut().trim_text(true);

    let mut slices = Vec::new();
    let mut current_tag: Option<String> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag.starts_with("slice") {
                    current_tag = Some(tag);
                }
            }
            Ok(Event::Text(e)) => {
                if let Some(tag) = &current_tag {
                    let text = e.unescape()?.into_owned();
                    if let Some(idx) = text.find("base64,") {
                        let b64 = &text[idx + "base64,".len()..];
                        let bytes = STANDARD.decode(b64.trim())?;
                        let n: u32 = tag.trim_start_matches("slice").parse().unwrap_or(0);
                        slices.push((n, bytes));
                    }
                }
            }
            Ok(Event::End(_)) => current_tag = None,
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow!("ошибка разбора XML: {e}")),
            _ => {}
        }
        buf.clear();
    }

    slices.sort_by_key(|(n, _)| *n);
    Ok(slices)
}

/// Склеивает вертикально набор изображений-кусочков в одну страницу.
fn splice_slices(slices: Vec<(u32, Vec<u8>)>) -> Result<DynamicImage> {
    let images: Vec<DynamicImage> = slices
        .into_iter()
        .map(|(_, bytes)| {
            image::load_from_memory(&bytes).context("не удалось декодировать срез страницы")
        })
        .collect::<Result<Vec<_>>>()?;

    let max_width = images.iter().map(|img| img.width()).max().unwrap_or(0);
    let total_height: u32 = images.iter().map(|img| img.height()).sum();

    let mut canvas = DynamicImage::new_rgb8(max_width, total_height);
    let mut y_offset = 0u32;
    for img in &images {
        canvas.copy_from(img, 0, y_offset)?;
        y_offset += img.height();
    }

    Ok(canvas)
}

/// Загружает одну книгу: определяет число страниц, перебирает их и перехватывает XHR-ответы.
async fn load_book(context: &playwright_rs::protocol::BrowserContext, link: &str) -> Result<()> {
    let page = context.new_page().await?;

    let book_num = link
        .split('=')
        .nth(1)
        .ok_or_else(|| anyhow!("не удалось извлечь номер книги из ссылки: {link}"))?
        .to_string();

    let pages_dir = PathBuf::from(format!("{book_num}_book_pages"));
    fs::create_dir_all(&pages_dir)?;

    // Аккумулятор XML-ответов постранично: pgnum -> сырые байты тела ответа
    let collected: Arc<Mutex<HashMap<u32, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));

    {
        let collected = collected.clone();
        page.on_response(move |response| {
            let collected = collected.clone();
            async move {
                let url = response.url().to_string();
                if url.contains("pgnum") {
                    if let Some(pg_part) = url.split("&pgnum=").nth(1) {
                        if let Ok(pgnum) = pg_part.parse::<u32>() {
                            if let Ok(body) = response.body().await {
                                collected.lock().await.insert(pgnum, body);
                            }
                        }
                    }
                }
                Ok(())
            }
        })
        .await?;
    }

    page.goto(link, None).await?;

    let total_pages_text = page
        .locator(
            "#body-root > div.controls > div > div > \
             div.controls__control-panel.control-panel.flex > \
             div.control-panel__pages.pages.flex > p",
        )
        .text_content()
        .await?
        .unwrap_or_default();

    let total_pages: u32 = total_pages_text
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0);

    for i in 0..total_pages {
        let page_out = pages_dir.join(format!("page_{i}.png"));
        if page_out.exists() {
            continue;
        }

        let input = page.locator("#page");
        input.wait_for(None).await.ok();
        input.clear(None).await.ok();
        input.fill(&i.to_string(), None).await.ok();
        input.press("Enter", None).await.ok();

        // Ждём поступления ответа с pgnum и небольшую паузу на дозагрузку срезов
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        if let Some(body) = collected.lock().await.remove(&i) {
            match parse_slices(&body) {
                Ok(slices) if !slices.is_empty() => {
                    let img = splice_slices(slices)?;
                    img.save(&page_out)?;
                }
                _ => eprintln!("Предупреждение: не удалось собрать страницу {i} книги {book_num}"),
            }
        }
    }

    let book_name = page
        .locator("#body-root > div.header > div > div > div > div > p > a")
        .text_content()
        .await?
        .unwrap_or_else(|| format!("book_{book_num}"));

    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    page.close().await?;

    create_pdf(&book_name, &pages_dir)?;

    Ok(())
}

/// Собирает все PNG-страницы из временной папки в один PDF-файл и удаляет папку.
fn create_pdf(name_book: &str, pages_dir: &Path) -> Result<()> {
    use printpdf::{ImageTransform, Mm, PdfDocument, image_crate::codecs::png::PngDecoder};

    let mut entries: Vec<PathBuf> = fs::read_dir(pages_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "png").unwrap_or(false))
        .collect();
    entries.sort_by_key(|p| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.trim_start_matches("page_").parse::<u32>().ok())
            .unwrap_or(0)
    });

    if entries.is_empty() {
        return Err(anyhow!(
            "нет страниц для сборки PDF: {}",
            pages_dir.display()
        ));
    }

    fs::create_dir_all("All-Books")?;

    let first = image::open(&entries[0])?;
    let (w_px, h_px) = (first.width() as f32, first.height() as f32);
    let (page_w, page_h) = (w_px * 25.4 / 96.0, h_px * 25.4 / 96.0);

    let (doc, page1, layer1) = PdfDocument::new(name_book, Mm(page_w), Mm(page_h), "Layer 1");

    for (idx, entry) in entries.iter().enumerate() {
        let (page_idx, layer_idx) = if idx == 0 {
            (page1, layer1)
        } else {
            doc.add_page(Mm(page_w), Mm(page_h), "Layer 1")
        };
        let layer = doc.get_page(page_idx).get_layer(layer_idx);

        let file = fs::File::open(entry)?;
        let decoder = PngDecoder::new(file)?;
        let img = printpdf::Image::try_from(decoder)?;
        img.add_to_layer(layer, ImageTransform::default());
    }

    let out_path = PathBuf::from("All-Books").join(format!("{name_book}.pdf"));
    doc.save(&mut std::io::BufWriter::new(fs::File::create(&out_path)?))?;

    fs::remove_dir_all(pages_dir)?;
    Ok(())
}
